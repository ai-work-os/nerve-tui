use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use nerve_tui_protocol::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub type PendingMap = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Result<Value>>>>>;

/// WebSocket client for nerve server.
pub struct NerveClient {
    /// Send JSON text to WS
    ws_tx: mpsc::UnboundedSender<String>,
    /// Pending request callbacks
    pending: PendingMap,
    /// Our registered node ID
    pub node_id: Option<String>,
    /// Our registered node name
    pub node_name: String,
}

impl NerveClient {
    /// Connect to nerve WS, register as UI node, return (client, event_rx).
    pub async fn connect(
        url: &str,
        name: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<NerveEvent>)> {
        info!("connecting to {}", url);
        let (ws_stream, _) = tokio_tungstenite::connect_async(url).await?;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<NerveEvent>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_read = pending.clone();

        // Writer task: forward ws_tx messages to WS sink
        tokio::spawn(async move {
            while let Some(text) = ws_rx.recv().await {
                if let Err(e) = ws_sink.send(Message::Text(text.into())).await {
                    error!("ws send error: {}", e);
                    break;
                }
            }
        });

        // Reader task: dispatch incoming WS messages
        tokio::spawn(async move {
            while let Some(msg) = ws_source.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        let text_str: &str = text.as_ref();
                        debug!("ws recv: {}", &text_str[..text_str.len().min(200)]);
                        match decode(text_str) {
                            Ok(rpc) => {
                                if rpc.is_response() {
                                    // Match to pending request
                                    if let Some(Value::Number(n)) = &rpc.id {
                                        if let Some(id) = n.as_u64() {
                                            let mut map = pending_read.lock().await;
                                            if let Some(tx) = map.remove(&id) {
                                                let result = if let Some(err) = rpc.error {
                                                    Err(anyhow!("{}: {}", err.code, err.message))
                                                } else {
                                                    Ok(rpc.result.unwrap_or(Value::Null))
                                                };
                                                let _ = tx.send(result);
                                            }
                                        }
                                    }
                                } else if rpc.is_notification() {
                                    let method = rpc.method.as_deref().unwrap_or("");
                                    let params = rpc.params.unwrap_or(Value::Null);
                                    if let Some(evt) = parse_notification(method, params) {
                                        let _ = event_tx.send(evt);
                                    }
                                }
                            }
                            Err(e) => warn!("ws decode error: {}", e),
                        }
                    }
                    Ok(Message::Close(_)) => {
                        let _ = event_tx.send(NerveEvent::Disconnected);
                        break;
                    }
                    Err(e) => {
                        error!("ws read error: {}", e);
                        let _ = event_tx.send(NerveEvent::Disconnected);
                        break;
                    }
                    _ => {}
                }
            }
        });

        let mut client = Self {
            ws_tx,
            pending,
            node_id: None,
            node_name: name.to_string(),
        };

        // Register as UI node
        let result = client
            .request(
                "node.register",
                json!({
                    "name": name,
                    "capabilities": ["ui"],
                    "permissions": "operator"
                }),
            )
            .await?;

        client.node_id = result
            .get("nodeId")
            .and_then(|v| v.as_str())
            .map(String::from);
        info!("registered as {} (nodeId: {:?})", name, client.node_id);

        Ok((client, event_rx))
    }

    /// Create a lightweight client from shared handles (for background tasks).
    pub fn from_parts(ws_tx: mpsc::UnboundedSender<String>, pending: PendingMap) -> Self {
        Self {
            ws_tx,
            pending,
            node_id: None,
            node_name: String::new(),
        }
    }

    /// Clone the WS sender for use in background tasks.
    pub fn ws_tx_clone(&self) -> mpsc::UnboundedSender<String> {
        self.ws_tx.clone()
    }

    /// Clone the pending map for use in background tasks.
    pub fn pending_clone(&self) -> PendingMap {
        self.pending.clone()
    }

    /// Send a JSON-RPC request and wait for response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (id, text) = encode_request(method, params);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, tx);
        }
        self.ws_tx
            .send(text)
            .map_err(|_| anyhow!("ws channel closed"))?;
        rx.await.map_err(|_| anyhow!("request cancelled"))?
    }

    // --- Convenience methods ---

    pub async fn channel_list(&self, cwd: Option<&str>) -> Result<Vec<ChannelInfo>> {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        let r = self.request("channel.list", params).await?;
        let channels: Vec<ChannelInfo> =
            serde_json::from_value(r.get("channels").cloned().unwrap_or(Value::Array(vec![])))?;
        Ok(channels)
    }

    pub async fn channel_create(
        &self,
        name: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<ChannelInfo> {
        let mut params = json!({});
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        let r = self.request("channel.create", params).await?;
        Ok(serde_json::from_value(r)?)
    }

    pub async fn channel_join(&self, channel_id: &str) -> Result<()> {
        self.request("channel.join", json!({ "channelId": channel_id }))
            .await?;
        Ok(())
    }

    pub async fn channel_post(&self, channel_id: &str, content: &str) -> Result<MessageInfo> {
        let r = self
            .request(
                "channel.post",
                json!({ "channelId": channel_id, "content": content }),
            )
            .await?;
        let msg: MessageInfo =
            serde_json::from_value(r.get("message").cloned().unwrap_or(r.clone()))?;
        Ok(msg)
    }

    pub async fn channel_history(
        &self,
        channel_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<MessageInfo>> {
        let mut params = json!({ "channelId": channel_id });
        if let Some(l) = limit {
            params["limit"] = json!(l);
        }
        let r = self.request("channel.history", params).await?;
        let msgs: Vec<MessageInfo> =
            serde_json::from_value(r.get("messages").cloned().unwrap_or(Value::Array(vec![])))?;
        Ok(msgs)
    }

    pub async fn node_list(&self, cwd: Option<&str>) -> Result<Vec<NodeInfo>> {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        let r = self.request("node.list", params).await?;
        let nodes: Vec<NodeInfo> =
            serde_json::from_value(r.get("nodes").cloned().unwrap_or(Value::Array(vec![])))?;
        Ok(nodes)
    }

    pub async fn find_process_nodes(&self, cwd: Option<&str>) -> Result<Vec<NodeInfo>> {
        debug!("requesting process nodes from node.list");
        let nodes = self.node_list(cwd).await?;
        Ok(nodes
            .into_iter()
            .filter(|node| node.transport == "stdio")
            .collect())
    }

    pub async fn node_spawn(
        &self,
        adapter: &str,
        name: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<SpawnResult> {
        let mut params = json!({ "adapter": adapter });
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        debug!(adapter, name, cwd, "requesting node.spawn");
        let r = self.request("node.spawn", params).await?;
        Ok(serde_json::from_value(r)?)
    }

    pub async fn node_stop(&self, node_id: &str) -> Result<()> {
        self.request("node.stop", json!({ "nodeId": node_id }))
            .await?;
        Ok(())
    }

    pub async fn node_prompt(&self, node_id: &str, content: &str) -> Result<PromptResult> {
        debug!(
            node_id,
            content_len = content.len(),
            "requesting node.prompt"
        );
        let r = self
            .request(
                "node.prompt",
                json!({ "nodeId": node_id, "content": content }),
            )
            .await?;
        Ok(serde_json::from_value(r)?)
    }

    pub async fn node_cancel(&self, node_id: &str) -> Result<()> {
        debug!(node_id, "requesting node.cancel");
        self.request("node.cancel", json!({ "nodeId": node_id }))
            .await?;
        Ok(())
    }

    pub async fn node_subscribe(&self, node_id: &str) -> Result<()> {
        debug!(node_id, "requesting node.subscribe");
        self.request("node.subscribe", json!({ "nodeId": node_id }))
            .await?;
        Ok(())
    }

    pub async fn node_unsubscribe(&self, node_id: &str) -> Result<()> {
        debug!(node_id, "requesting node.unsubscribe");
        self.request("node.unsubscribe", json!({ "nodeId": node_id }))
            .await?;
        Ok(())
    }

    pub async fn channel_list_archived(&self, cwd: Option<&str>) -> Result<Vec<Value>> {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        let r = self.request("channel.listArchived", params).await?;
        let channels: Vec<Value> =
            serde_json::from_value(r.get("channels").cloned().unwrap_or(Value::Array(vec![])))?;
        Ok(channels)
    }

    pub async fn channel_restore(&self, channel_id: &str) -> Result<ChannelInfo> {
        let r = self
            .request("channel.restore", json!({ "channelId": channel_id }))
            .await?;
        Ok(serde_json::from_value(r)?)
    }

    pub async fn session_clear(&self, node_name: &str) -> Result<Value> {
        let r = self
            .request("session.clear", json!({ "nodeName": node_name }))
            .await?;
        Ok(r)
    }

    pub async fn session_compact(&self, node_name: &str) -> Result<()> {
        self.request("session.compact", json!({ "nodeName": node_name }))
            .await?;
        Ok(())
    }

    pub async fn channel_add_node(
        &self,
        channel_id: &str,
        node_id: &str,
        name: Option<&str>,
    ) -> Result<()> {
        let mut params = json!({ "channelId": channel_id, "nodeId": node_id });
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        self.request("channel.addNode", params).await?;
        Ok(())
    }
}

fn parse_notification(method: &str, params: Value) -> Option<NerveEvent> {
    match method {
        "channel.message" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let message: MessageInfo =
                serde_json::from_value(params.get("message")?.clone()).ok()?;
            Some(NerveEvent::ChannelMessage {
                channel_id,
                message,
            })
        }
        "channel.mention" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let message: MessageInfo =
                serde_json::from_value(params.get("message")?.clone()).ok()?;
            Some(NerveEvent::ChannelMention {
                channel_id,
                message,
            })
        }
        "channel.nodeJoined" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let node_name = params.get("nodeName")?.as_str()?.to_string();
            Some(NerveEvent::NodeJoined {
                channel_id,
                node_id,
                node_name,
            })
        }
        "channel.nodeLeft" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let node_name = params.get("nodeName")?.as_str()?.to_string();
            Some(NerveEvent::NodeLeft {
                channel_id,
                node_id,
                node_name,
            })
        }
        "node.update" => {
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let name = params.get("name")?.as_str()?.to_string();
            Some(NerveEvent::NodeUpdate {
                node_id,
                name,
                detail: params,
            })
        }
        "node.statusChanged" => {
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let name = params.get("name")?.as_str()?.to_string();
            let status = params.get("status")?.as_str()?.to_string();
            let activity = params
                .get("activity")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(NerveEvent::NodeStatusChanged {
                node_id,
                name,
                status,
                activity,
            })
        }
        "node.registered" => {
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let name = params.get("name")?.as_str()?.to_string();
            let adapter = params
                .get("adapter")
                .and_then(|v| v.as_str())
                .map(String::from);
            let transport = params
                .get("transport")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(NerveEvent::NodeRegistered {
                node_id,
                name,
                adapter,
                transport,
            })
        }
        "node.stopped" => {
            let node_id = params.get("nodeId")?.as_str()?.to_string();
            let name = params.get("name")?.as_str()?.to_string();
            Some(NerveEvent::NodeStopped { node_id, name })
        }
        "channel.created" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(NerveEvent::ChannelCreated { channel_id, name })
        }
        "channel.closed" => {
            let channel_id = params.get("channelId")?.as_str()?.to_string();
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(NerveEvent::ChannelClosed { channel_id, name })
        }
        _ => {
            warn!("unknown notification: {}", method);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_channel_message() {
        let params = json!({
            "channelId": "ch1",
            "message": {
                "id": "m1",
                "channelId": "ch1",
                "from": "alice",
                "content": "hello",
                "timestamp": 1710000000.0,
            }
        });
        let evt = parse_notification("channel.message", params).unwrap();
        match evt {
            NerveEvent::ChannelMessage {
                channel_id,
                message,
            } => {
                assert_eq!(channel_id, "ch1");
                assert_eq!(message.from, "alice");
                assert_eq!(message.content, "hello");
            }
            _ => panic!("expected ChannelMessage"),
        }
    }

    #[test]
    fn parse_channel_mention() {
        let params = json!({
            "channelId": "ch1",
            "message": {
                "id": "m2",
                "channelId": "ch1",
                "from": "bob",
                "content": "@tui check this",
                "timestamp": 1710000000.0,
            }
        });
        let evt = parse_notification("channel.mention", params).unwrap();
        match evt {
            NerveEvent::ChannelMention {
                channel_id,
                message,
            } => {
                assert_eq!(channel_id, "ch1");
                assert_eq!(message.from, "bob");
            }
            _ => panic!("expected ChannelMention"),
        }
    }

    #[test]
    fn parse_node_joined() {
        let params = json!({
            "channelId": "ch1",
            "nodeId": "n1",
            "nodeName": "alice",
        });
        let evt = parse_notification("channel.nodeJoined", params).unwrap();
        match evt {
            NerveEvent::NodeJoined {
                channel_id,
                node_id,
                node_name,
            } => {
                assert_eq!(channel_id, "ch1");
                assert_eq!(node_id, "n1");
                assert_eq!(node_name, "alice");
            }
            _ => panic!("expected NodeJoined"),
        }
    }

    #[test]
    fn parse_node_left() {
        let params = json!({
            "channelId": "ch1",
            "nodeId": "n1",
            "nodeName": "alice",
        });
        let evt = parse_notification("channel.nodeLeft", params).unwrap();
        match evt {
            NerveEvent::NodeLeft {
                channel_id,
                node_id,
                node_name,
            } => {
                assert_eq!(channel_id, "ch1");
                assert_eq!(node_id, "n1");
                assert_eq!(node_name, "alice");
            }
            _ => panic!("expected NodeLeft"),
        }
    }

    #[test]
    fn parse_node_update() {
        let params = json!({
            "nodeId": "n1",
            "name": "alice",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"text": "hello"},
            }
        });
        let evt = parse_notification("node.update", params).unwrap();
        match evt {
            NerveEvent::NodeUpdate {
                node_id,
                name,
                detail,
            } => {
                assert_eq!(node_id, "n1");
                assert_eq!(name, "alice");
                assert!(detail.get("update").is_some());
            }
            _ => panic!("expected NodeUpdate"),
        }
    }

    #[test]
    fn parse_node_update_from_real_server_notification_shape() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "method":"node.update",
            "params":{
                "nodeId":"n1",
                "name":"alice",
                "update":{
                    "sessionUpdate":"user_message",
                    "content":{"type":"text","text":"direct ping"}
                }
            }
        }"#;
        let rpc = decode(raw).unwrap();
        assert!(rpc.is_notification());

        let evt = parse_notification(rpc.method.as_deref().unwrap(), rpc.params.unwrap()).unwrap();

        match evt {
            NerveEvent::NodeUpdate {
                node_id,
                name,
                detail,
            } => {
                assert_eq!(node_id, "n1");
                assert_eq!(name, "alice");
                assert_eq!(
                    detail["update"]["sessionUpdate"].as_str(),
                    Some("user_message")
                );
                assert_eq!(
                    detail["update"]["content"]["text"].as_str(),
                    Some("direct ping")
                );
            }
            _ => panic!("expected NodeUpdate"),
        }
    }

    #[test]
    fn parse_node_status_changed() {
        let params = json!({
            "nodeId": "n1",
            "name": "alice",
            "status": "busy",
            "activity": "thinking",
        });
        let evt = parse_notification("node.statusChanged", params).unwrap();
        match evt {
            NerveEvent::NodeStatusChanged {
                node_id,
                name,
                status,
                activity,
            } => {
                assert_eq!(node_id, "n1");
                assert_eq!(name, "alice");
                assert_eq!(status, "busy");
                assert_eq!(activity, Some("thinking".to_string()));
            }
            _ => panic!("expected NodeStatusChanged"),
        }
    }

    #[test]
    fn parse_node_status_changed_no_activity() {
        let params = json!({
            "nodeId": "n1",
            "name": "alice",
            "status": "idle",
        });
        let evt = parse_notification("node.statusChanged", params).unwrap();
        match evt {
            NerveEvent::NodeStatusChanged { activity, .. } => {
                assert!(activity.is_none());
            }
            _ => panic!("expected NodeStatusChanged"),
        }
    }

    #[test]
    fn parse_unknown_method_returns_none() {
        let result = parse_notification("unknown.method", json!({}));
        assert!(result.is_none());
    }

    #[test]
    fn parse_missing_fields_returns_none() {
        // channel.message without "message" field
        let result = parse_notification("channel.message", json!({"channelId": "ch1"}));
        assert!(result.is_none());

        // node.statusChanged without "status"
        let result = parse_notification("node.statusChanged", json!({"nodeId": "n1", "name": "a"}));
        assert!(result.is_none());
    }

    #[test]
    fn parse_channel_message_bad_message_returns_none() {
        let params = json!({
            "channelId": "ch1",
            "message": "not an object",
        });
        let result = parse_notification("channel.message", params);
        assert!(result.is_none());
    }

    #[test]
    fn node_spawn_response_deserializes_to_spawn_result() {
        let result: SpawnResult = serde_json::from_value(json!({
            "nodeId": "node-1",
            "name": "worker",
        }))
        .unwrap();
        assert_eq!(
            result,
            SpawnResult {
                node_id: "node-1".into(),
                name: "worker".into(),
            }
        );
    }

    #[test]
    fn prompt_response_deserializes_to_prompt_result() {
        let result: PromptResult = serde_json::from_value(json!({
            "stopReason": "end_turn",
        }))
        .unwrap();
        assert_eq!(
            result,
            PromptResult {
                stop_reason: Some("end_turn".into()),
            }
        );
    }

    #[test]
    fn unsubscribe_request_encodes_expected_payload() {
        let (id, text) = encode_request("node.unsubscribe", json!({ "nodeId": "node-1" }));
        let rpc = decode(&text).unwrap();
        assert_eq!(rpc.id, Some(Value::Number(id.into())));
        assert_eq!(rpc.method.as_deref(), Some("node.unsubscribe"));
        assert_eq!(rpc.params, Some(json!({ "nodeId": "node-1" })));
    }

    #[test]
    fn cancel_request_encodes_expected_payload() {
        let (id, text) = encode_request("node.cancel", json!({ "nodeId": "node-1" }));
        let rpc = decode(&text).unwrap();
        assert_eq!(rpc.id, Some(Value::Number(id.into())));
        assert_eq!(rpc.method.as_deref(), Some("node.cancel"));
        assert_eq!(rpc.params, Some(json!({ "nodeId": "node-1" })));
    }

    #[test]
    fn parse_channel_created() {
        let params = json!({
            "channelId": "ch-new",
            "name": "my-channel",
            "cwd": "/tmp",
        });
        let evt = parse_notification("channel.created", params).unwrap();
        match evt {
            NerveEvent::ChannelCreated { channel_id, name } => {
                assert_eq!(channel_id, "ch-new");
                assert_eq!(name.as_deref(), Some("my-channel"));
            }
            _ => panic!("expected ChannelCreated"),
        }
    }

    #[test]
    fn parse_channel_created_no_name() {
        let params = json!({ "channelId": "ch-noname", "cwd": "/tmp" });
        let evt = parse_notification("channel.created", params).unwrap();
        match evt {
            NerveEvent::ChannelCreated { channel_id, name } => {
                assert_eq!(channel_id, "ch-noname");
                assert!(name.is_none());
            }
            _ => panic!("expected ChannelCreated"),
        }
    }

    #[test]
    fn parse_channel_closed() {
        let params = json!({
            "channelId": "ch-old",
            "name": "old-channel",
            "cwd": "/tmp",
        });
        let evt = parse_notification("channel.closed", params).unwrap();
        match evt {
            NerveEvent::ChannelClosed { channel_id, name } => {
                assert_eq!(channel_id, "ch-old");
                assert_eq!(name.as_deref(), Some("old-channel"));
            }
            _ => panic!("expected ChannelClosed"),
        }
    }

    #[test]
    fn parse_channel_closed_no_name() {
        let params = json!({ "channelId": "ch-unnamed", "cwd": "/tmp" });
        let evt = parse_notification("channel.closed", params).unwrap();
        match evt {
            NerveEvent::ChannelClosed { channel_id, name } => {
                assert_eq!(channel_id, "ch-unnamed");
                assert!(name.is_none());
            }
            _ => panic!("expected ChannelClosed"),
        }
    }

    #[test]
    fn find_process_nodes_filters_websocket_nodes() {
        let nodes = vec![
            NodeInfo {
                id: "n1".into(),
                name: "agent".into(),
                status: "idle".into(),
                capabilities: vec![],
                permissions: "member".into(),
                transport: "stdio".into(),
                adapter: Some("claude".into()),
                channels: vec![],
                created_at: 0.0,
                last_active_at: 0.0,
            },
            NodeInfo {
                id: "n2".into(),
                name: "tui".into(),
                status: "idle".into(),
                capabilities: vec![],
                permissions: "operator".into(),
                transport: "websocket".into(),
                adapter: None,
                channels: vec![],
                created_at: 0.0,
                last_active_at: 0.0,
            },
        ];

        let process_nodes: Vec<NodeInfo> = nodes
            .into_iter()
            .filter(|node| node.transport == "stdio")
            .collect();

        assert_eq!(process_nodes.len(), 1);
        assert_eq!(process_nodes[0].name, "agent");
    }
}
