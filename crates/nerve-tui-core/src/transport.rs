use anyhow::Result;
use nerve_tui_protocol::*;
use serde_json::{json, Value};
use std::future::Future;

/// Abstraction over the nerve server communication layer.
/// App depends on this trait instead of NerveClient directly.
pub trait Transport: Clone + Send + Sync + 'static {
    /// Send a JSON-RPC request and wait for response.
    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> impl Future<Output = Result<Value>> + Send;

    /// The registered node name.
    fn node_name(&self) -> &str;

    // --- Default convenience methods (delegate to request) ---

    fn channel_list(&self, cwd: Option<&str>) -> impl Future<Output = Result<Vec<ChannelInfo>>> + Send {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move {
            let r = self.request("channel.list", params).await?;
            let channels: Vec<ChannelInfo> =
                serde_json::from_value(r.get("channels").cloned().unwrap_or(Value::Array(vec![])))?;
            Ok(channels)
        }
    }

    fn channel_create(
        &self,
        name: Option<&str>,
        cwd: Option<&str>,
    ) -> impl Future<Output = Result<ChannelInfo>> + Send {
        let mut params = json!({});
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move {
            let r = self.request("channel.create", params).await?;
            Ok(serde_json::from_value(r)?)
        }
    }

    fn channel_join(&self, channel_id: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "channelId": channel_id });
        async move {
            self.request("channel.join", params).await?;
            Ok(())
        }
    }

    fn channel_post(
        &self,
        channel_id: &str,
        content: &str,
    ) -> impl Future<Output = Result<MessageInfo>> + Send {
        let tagged = format!("{}: {}", self.node_name(), content);
        let params = json!({ "channelId": channel_id, "content": tagged });
        async move {
            let r = self.request("channel.post", params).await?;
            let msg: MessageInfo =
                serde_json::from_value(r.get("message").cloned().unwrap_or(r.clone()))?;
            Ok(msg)
        }
    }

    fn channel_history(
        &self,
        channel_id: &str,
        limit: Option<u32>,
    ) -> impl Future<Output = Result<Vec<MessageInfo>>> + Send {
        let mut params = json!({ "channelId": channel_id });
        if let Some(l) = limit {
            params["limit"] = json!(l);
        }
        async move {
            let r = self.request("channel.history", params).await?;
            let msgs: Vec<MessageInfo> =
                serde_json::from_value(r.get("messages").cloned().unwrap_or(Value::Array(vec![])))?;
            Ok(msgs)
        }
    }

    fn node_list(&self, cwd: Option<&str>) -> impl Future<Output = Result<Vec<NodeInfo>>> + Send {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move {
            let r = self.request("node.list", params).await?;
            let nodes: Vec<NodeInfo> =
                serde_json::from_value(r.get("nodes").cloned().unwrap_or(Value::Array(vec![])))?;
            Ok(nodes)
        }
    }

    fn node_spawn(
        &self,
        adapter: &str,
        name: Option<&str>,
        cwd: Option<&str>,
    ) -> impl Future<Output = Result<SpawnResult>> + Send {
        let mut params = json!({ "adapter": adapter });
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move {
            let r = self.request("node.spawn", params).await?;
            Ok(serde_json::from_value(r)?)
        }
    }

    fn node_stop(&self, node_id: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "nodeId": node_id });
        async move {
            self.request("node.stop", params).await?;
            Ok(())
        }
    }

    fn node_prompt(
        &self,
        node_id: &str,
        content: &str,
    ) -> impl Future<Output = Result<PromptResult>> + Send {
        let tagged = format!("{}: {}", self.node_name(), content);
        let params = json!({ "nodeId": node_id, "content": tagged });
        async move {
            let r = self.request("node.prompt", params).await?;
            Ok(serde_json::from_value(r)?)
        }
    }

    fn node_message(&self, node_id: &str, content: &str) -> impl Future<Output = Result<()>> + Send {
        let tagged = format!("{}: {}", self.node_name(), content);
        let params = json!({ "nodeId": node_id, "content": tagged });
        async move {
            self.request("node.message", params).await?;
            Ok(())
        }
    }

    fn node_cancel(&self, node_id: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "nodeId": node_id });
        async move {
            self.request("node.cancel", params).await?;
            Ok(())
        }
    }

    fn node_subscribe(&self, node_id: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "nodeId": node_id });
        async move {
            self.request("node.subscribe", params).await?;
            Ok(())
        }
    }

    fn node_unsubscribe(&self, node_id: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "nodeId": node_id });
        async move {
            self.request("node.unsubscribe", params).await?;
            Ok(())
        }
    }

    fn session_clear(&self, node_name: &str) -> impl Future<Output = Result<Value>> + Send {
        let params = json!({ "nodeName": node_name });
        async move { self.request("session.clear", params).await }
    }

    fn session_compact(&self, node_name: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "nodeName": node_name });
        async move {
            self.request("session.compact", params).await?;
            Ok(())
        }
    }

    fn find_process_nodes(&self, cwd: Option<&str>) -> impl Future<Output = Result<Vec<NodeInfo>>> + Send {
        async move {
            let nodes = self.node_list(cwd).await?;
            Ok(nodes.into_iter().filter(|n| n.transport == "stdio").collect())
        }
    }

    fn channel_list_archived(&self, cwd: Option<&str>) -> impl Future<Output = Result<Vec<Value>>> + Send {
        let mut params = json!({});
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move {
            let r = self.request("channel.listArchived", params).await?;
            let channels: Vec<Value> =
                serde_json::from_value(r.get("channels").cloned().unwrap_or(Value::Array(vec![])))?;
            Ok(channels)
        }
    }

    fn channel_restore(&self, channel_id: &str) -> impl Future<Output = Result<ChannelInfo>> + Send {
        let params = json!({ "channelId": channel_id });
        async move {
            let r = self.request("channel.restore", params).await?;
            Ok(serde_json::from_value(r)?)
        }
    }

    fn channel_add_node(
        &self,
        channel_id: &str,
        node_id: &str,
        name: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send {
        let mut params = json!({ "channelId": channel_id, "nodeId": node_id });
        if let Some(n) = name {
            params["name"] = json!(n);
        }
        async move {
            self.request("channel.addNode", params).await?;
            Ok(())
        }
    }

    fn scene_list(&self) -> impl Future<Output = Result<Vec<Value>>> + Send {
        async move {
            let r = self.request("scene.list", json!({})).await?;
            let scenes = r.get("scenes").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            Ok(scenes)
        }
    }

    fn scene_start(&self, name: &str, cwd: Option<&str>) -> impl Future<Output = Result<Value>> + Send {
        let mut params = json!({ "name": name });
        if let Some(c) = cwd {
            params["cwd"] = json!(c);
        }
        async move { self.request("scene.start", params).await }
    }

    fn scene_stop(&self, name: &str) -> impl Future<Output = Result<()>> + Send {
        let params = json!({ "name": name });
        async move {
            self.request("scene.stop", params).await?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records each (method, params) call and returns a preset response or error.
    #[derive(Clone)]
    struct MockTransport {
        name: String,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        response: Arc<Mutex<Value>>,
        error: Arc<Mutex<Option<String>>>,
    }

    impl MockTransport {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                calls: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(Value::Null)),
                error: Arc::new(Mutex::new(None)),
            }
        }

        fn set_response(&self, v: Value) {
            *self.response.lock().unwrap() = v;
            *self.error.lock().unwrap() = None;
        }

        fn set_error(&self, msg: &str) {
            *self.error.lock().unwrap() = Some(msg.to_string());
        }

        fn last_call(&self) -> (String, Value) {
            self.calls.lock().unwrap().last().cloned().unwrap()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Transport for MockTransport {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            if let Some(err) = self.error.lock().unwrap().as_ref() {
                return Err(anyhow::anyhow!("{}", err));
            }
            Ok(self.response.lock().unwrap().clone())
        }

        fn node_name(&self) -> &str {
            &self.name
        }
    }

    // --- Test: MockTransport implements Transport ---

    #[tokio::test]
    async fn mock_transport_implements_trait() {
        let t = MockTransport::new("test-node");
        assert_eq!(t.node_name(), "test-node");

        t.set_response(json!({"ok": true}));
        let r = t.request("ping", json!({})).await.unwrap();
        assert_eq!(r, json!({"ok": true}));
        assert_eq!(t.call_count(), 1);
    }

    // --- Test: channel_list calls correct method ---

    #[tokio::test]
    async fn channel_list_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "channels": [] }));

        let result = t.channel_list(None).await.unwrap();
        assert!(result.is_empty());

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.list");
        assert_eq!(params, json!({}));
    }

    #[tokio::test]
    async fn channel_list_with_cwd() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "channels": [] }));

        t.channel_list(Some("/tmp")).await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.list");
        assert_eq!(params["cwd"], json!("/tmp"));
    }

    // --- Test: channel_join ---

    #[tokio::test]
    async fn channel_join_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.channel_join("ch-1").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.join");
        assert_eq!(params["channelId"], json!("ch-1"));
    }

    // --- Test: channel_post ---

    #[tokio::test]
    async fn channel_post_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({
            "message": {
                "id": "m1", "channelId": "ch-1", "from": "n",
                "content": "n: hello", "timestamp": 1.0
            }
        }));

        let msg = t.channel_post("ch-1", "hello").await.unwrap();
        assert_eq!(msg.content, "n: hello");

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.post");
        assert_eq!(params["channelId"], json!("ch-1"));
        assert_eq!(params["content"], json!("n: hello"));
    }

    // --- Test: node_subscribe ---

    #[tokio::test]
    async fn node_subscribe_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.node_subscribe("node-1").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "node.subscribe");
        assert_eq!(params["nodeId"], json!("node-1"));
    }

    // --- Test: node_spawn ---

    #[tokio::test]
    async fn node_spawn_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "nodeId": "n-new", "name": "worker" }));

        let r = t.node_spawn("claude", Some("worker"), None).await.unwrap();
        assert_eq!(r.node_id, "n-new");
        assert_eq!(r.name, "worker");

        let (method, params) = t.last_call();
        assert_eq!(method, "node.spawn");
        assert_eq!(params["adapter"], json!("claude"));
        assert_eq!(params["name"], json!("worker"));
    }

    // --- Test: node_prompt ---

    #[tokio::test]
    async fn node_prompt_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "stopReason": "end_turn" }));

        let r = t.node_prompt("node-1", "do something").await.unwrap();
        assert_eq!(r.stop_reason, Some("end_turn".to_string()));

        let (method, params) = t.last_call();
        assert_eq!(method, "node.prompt");
        assert_eq!(params["nodeId"], json!("node-1"));
        assert_eq!(params["content"], json!("n: do something"));
    }

    // --- Test: session_clear ---

    #[tokio::test]
    async fn session_clear_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "cleared": true }));

        let r = t.session_clear("my-node").await.unwrap();
        assert_eq!(r, json!({ "cleared": true }));

        let (method, params) = t.last_call();
        assert_eq!(method, "session.clear");
        assert_eq!(params["nodeName"], json!("my-node"));
    }

    // --- Test: channel_create ---

    #[tokio::test]
    async fn channel_create_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "id": "ch-new", "name": "test", "cwd": "/tmp" }));

        let ch = t.channel_create(Some("test"), Some("/tmp")).await.unwrap();
        assert_eq!(ch.id, "ch-new");

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.create");
        assert_eq!(params["name"], json!("test"));
        assert_eq!(params["cwd"], json!("/tmp"));
    }

    // --- Test: channel_history ---

    #[tokio::test]
    async fn channel_history_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "messages": [] }));

        let msgs = t.channel_history("ch-1", Some(10)).await.unwrap();
        assert!(msgs.is_empty());

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.history");
        assert_eq!(params["channelId"], json!("ch-1"));
        assert_eq!(params["limit"], json!(10));
    }

    // --- Test: node_list ---

    #[tokio::test]
    async fn node_list_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "nodes": [] }));

        let nodes = t.node_list(None).await.unwrap();
        assert!(nodes.is_empty());

        let (method, _) = t.last_call();
        assert_eq!(method, "node.list");
    }

    // --- Test: node_stop ---

    #[tokio::test]
    async fn node_stop_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.node_stop("n-1").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "node.stop");
        assert_eq!(params["nodeId"], json!("n-1"));
    }

    // --- Test: node_message ---

    #[tokio::test]
    async fn node_message_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.node_message("n-1", "hello").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "node.message");
        assert_eq!(params["nodeId"], json!("n-1"));
        assert_eq!(params["content"], json!("n: hello"));
    }

    // --- Test: node_cancel ---

    #[tokio::test]
    async fn node_cancel_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.node_cancel("n-1").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "node.cancel");
        assert_eq!(params["nodeId"], json!("n-1"));
    }

    // --- Test: node_unsubscribe ---

    #[tokio::test]
    async fn node_unsubscribe_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.node_unsubscribe("n-1").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "node.unsubscribe");
        assert_eq!(params["nodeId"], json!("n-1"));
    }

    // --- Test: session_compact ---

    #[tokio::test]
    async fn session_compact_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.session_compact("my-node").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "session.compact");
        assert_eq!(params["nodeName"], json!("my-node"));
    }

    // --- Test: find_process_nodes filters by stdio ---

    #[tokio::test]
    async fn find_process_nodes_filters_stdio() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "nodes": [
            { "id": "n1", "name": "agent", "status": "idle", "transport": "stdio", "permissions": "member", "createdAt": 0, "lastActiveAt": 0 },
            { "id": "n2", "name": "tui", "status": "idle", "transport": "websocket", "permissions": "operator", "createdAt": 0, "lastActiveAt": 0 }
        ]}));

        let nodes = t.find_process_nodes(None).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "agent");
    }

    // --- Test: channel_list_archived ---

    #[tokio::test]
    async fn channel_list_archived_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "channels": [{"id": "old"}] }));

        let archived = t.channel_list_archived(None).await.unwrap();
        assert_eq!(archived.len(), 1);

        let (method, _) = t.last_call();
        assert_eq!(method, "channel.listArchived");
    }

    // --- Test: channel_restore ---

    #[tokio::test]
    async fn channel_restore_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "id": "ch-old", "cwd": "/tmp" }));

        let ch = t.channel_restore("ch-old").await.unwrap();
        assert_eq!(ch.id, "ch-old");

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.restore");
        assert_eq!(params["channelId"], json!("ch-old"));
    }

    // --- Test: channel_add_node ---

    #[tokio::test]
    async fn channel_add_node_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.channel_add_node("ch-1", "n-1", Some("worker")).await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "channel.addNode");
        assert_eq!(params["channelId"], json!("ch-1"));
        assert_eq!(params["nodeId"], json!("n-1"));
        assert_eq!(params["name"], json!("worker"));
    }

    // --- Test: scene_list ---

    #[tokio::test]
    async fn scene_list_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "scenes": [{"name": "dev"}] }));

        let scenes = t.scene_list().await.unwrap();
        assert_eq!(scenes.len(), 1);

        let (method, _) = t.last_call();
        assert_eq!(method, "scene.list");
    }

    // --- Test: scene_start ---

    #[tokio::test]
    async fn scene_start_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({ "started": true }));

        let r = t.scene_start("dev", Some("/work")).await.unwrap();
        assert_eq!(r["started"], json!(true));

        let (method, params) = t.last_call();
        assert_eq!(method, "scene.start");
        assert_eq!(params["name"], json!("dev"));
        assert_eq!(params["cwd"], json!("/work"));
    }

    // --- Test: scene_stop ---

    #[tokio::test]
    async fn scene_stop_calls_correct_method() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.scene_stop("dev").await.unwrap();

        let (method, params) = t.last_call();
        assert_eq!(method, "scene.stop");
        assert_eq!(params["name"], json!("dev"));
    }

    // --- Test: error path ---

    #[tokio::test]
    async fn request_error_propagates_to_convenience_methods() {
        let t = MockTransport::new("n");
        t.set_error("connection refused");

        let err = t.channel_list(None).await.unwrap_err();
        assert!(err.to_string().contains("connection refused"));

        let err = t.node_subscribe("n-1").await.unwrap_err();
        assert!(err.to_string().contains("connection refused"));

        let err = t.scene_list().await.unwrap_err();
        assert!(err.to_string().contains("connection refused"));
    }

    // --- Test: multiple calls accumulate ---

    #[tokio::test]
    async fn multiple_calls_are_recorded() {
        let t = MockTransport::new("n");
        t.set_response(json!({}));

        t.channel_join("ch-1").await.unwrap();
        t.node_subscribe("n-1").await.unwrap();
        t.node_cancel("n-1").await.unwrap();

        assert_eq!(t.call_count(), 3);

        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "channel.join");
        assert_eq!(calls[1].0, "node.subscribe");
        assert_eq!(calls[2].0, "node.cancel");
    }
}
