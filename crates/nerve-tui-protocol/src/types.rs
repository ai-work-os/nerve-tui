use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Node ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub transport: String,
    pub adapter: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub last_active_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnResult {
    pub node_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DmMessage {
    pub role: String,
    pub content: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DmState {
    pub node_id: String,
    pub node_name: String,
    pub messages: Vec<DmMessage>,
    pub streaming: Option<String>,
    pub is_responding: bool,
}

// --- Channel ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelInfo {
    pub id: String,
    pub name: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub nodes: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub created_at: f64,
}

// --- Message ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageInfo {
    pub id: String,
    pub channel_id: String,
    pub from: String,
    pub content: String,
    pub timestamp: f64,
    #[serde(default)]
    pub metadata: Option<Value>,
}

// --- WS Notifications ---

#[derive(Debug, Clone)]
pub enum NerveEvent {
    /// New message in channel
    ChannelMessage {
        channel_id: String,
        message: MessageInfo,
    },
    /// Current node was @mentioned
    ChannelMention {
        channel_id: String,
        message: MessageInfo,
    },
    /// Node joined channel
    NodeJoined {
        channel_id: String,
        node_id: String,
        node_name: String,
    },
    /// Node left channel
    NodeLeft {
        channel_id: String,
        node_id: String,
        node_name: String,
    },
    /// Agent streaming output
    NodeUpdate {
        node_id: String,
        name: String,
        detail: Value,
    },
    /// Agent status changed
    NodeStatusChanged {
        node_id: String,
        name: String,
        status: String,
        activity: Option<String>,
    },
    /// A new channel was created
    ChannelCreated {
        channel_id: String,
        name: Option<String>,
    },
    /// A channel was closed
    ChannelClosed {
        channel_id: String,
        name: Option<String>,
    },
    /// A new node was registered (spawned or connected)
    NodeRegistered {
        node_id: String,
        name: String,
        adapter: Option<String>,
        transport: Option<String>,
    },
    /// Agent process stopped (crash or explicit stop)
    NodeStopped {
        node_id: String,
        name: String,
    },
    /// WS connection lost
    Disconnected,
}

impl NerveEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            NerveEvent::ChannelMessage { .. } => "ChannelMessage",
            NerveEvent::ChannelMention { .. } => "ChannelMention",
            NerveEvent::NodeJoined { .. } => "NodeJoined",
            NerveEvent::NodeLeft { .. } => "NodeLeft",
            NerveEvent::NodeUpdate { .. } => "NodeUpdate",
            NerveEvent::NodeStatusChanged { .. } => "NodeStatusChanged",
            NerveEvent::ChannelCreated { .. } => "ChannelCreated",
            NerveEvent::ChannelClosed { .. } => "ChannelClosed",
            NerveEvent::NodeRegistered { .. } => "NodeRegistered",
            NerveEvent::NodeStopped { .. } => "NodeStopped",
            NerveEvent::Disconnected => "Disconnected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_info_deserialize_camel_case() {
        let json = json!({
            "id": "n1",
            "name": "alice",
            "status": "idle",
            "capabilities": ["ui"],
            "permissions": "operator",
            "transport": "stdio",
            "adapter": "claude",
            "channels": ["ch1"],
            "createdAt": 1710000000.0,
            "lastActiveAt": 1710000100.0,
        });
        let node: NodeInfo = serde_json::from_value(json).unwrap();
        assert_eq!(node.id, "n1");
        assert_eq!(node.name, "alice");
        assert_eq!(node.status, "idle");
        assert_eq!(node.adapter, Some("claude".to_string()));
        assert_eq!(node.channels, vec!["ch1"]);
    }

    #[test]
    fn node_info_deserialize_defaults() {
        let json = json!({
            "id": "n2",
            "name": "bob",
            "status": "busy",
        });
        let node: NodeInfo = serde_json::from_value(json).unwrap();
        assert_eq!(node.capabilities, Vec::<String>::new());
        assert_eq!(node.permissions, "");
        assert_eq!(node.transport, "");
        assert!(node.adapter.is_none());
        assert_eq!(node.channels, Vec::<String>::new());
    }

    #[test]
    fn channel_info_deserialize() {
        let json = json!({
            "id": "ch1",
            "name": "main",
            "cwd": "/tmp",
            "nodes": {"alice": "n1", "bob": "n2"},
            "createdAt": 1710000000.0,
        });
        let ch: ChannelInfo = serde_json::from_value(json).unwrap();
        assert_eq!(ch.id, "ch1");
        assert_eq!(ch.name, Some("main".to_string()));
        assert_eq!(ch.nodes.len(), 2);
        assert_eq!(ch.nodes.get("alice"), Some(&"n1".to_string()));
    }

    #[test]
    fn channel_info_optional_name() {
        let json = json!({"id": "ch2", "cwd": "/home"});
        let ch: ChannelInfo = serde_json::from_value(json).unwrap();
        assert!(ch.name.is_none());
    }

    #[test]
    fn message_info_roundtrip() {
        let msg = MessageInfo {
            id: "m1".into(),
            channel_id: "ch1".into(),
            from: "alice".into(),
            content: "@bob hello".into(),
            timestamp: 1710000000.0,
            metadata: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["channelId"], "ch1");
        assert_eq!(json["from"], "alice");

        let back: MessageInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.id, "m1");
        assert_eq!(back.content, "@bob hello");
    }

    #[test]
    fn message_info_with_metadata() {
        let json = json!({
            "id": "m2",
            "channelId": "ch1",
            "from": "sys",
            "content": "test",
            "timestamp": 0.0,
            "metadata": {"key": "val"},
        });
        let msg: MessageInfo = serde_json::from_value(json).unwrap();
        assert!(msg.metadata.is_some());
    }

    #[test]
    fn node_info_serialize_camel_case() {
        let node = NodeInfo {
            id: "n1".into(),
            name: "test".into(),
            status: "idle".into(),
            capabilities: vec![],
            permissions: "operator".into(),
            transport: "websocket".into(),
            adapter: None,
            channels: vec![],
            created_at: 0.0,
            last_active_at: 0.0,
        };
        let json = serde_json::to_value(&node).unwrap();
        assert!(json.get("createdAt").is_some());
        assert!(json.get("lastActiveAt").is_some());
        // Should NOT have snake_case
        assert!(json.get("created_at").is_none());
    }

    #[test]
    fn spawn_result_deserialize_camel_case() {
        let json = json!({
            "nodeId": "n123",
            "name": "alice",
        });
        let result: SpawnResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.node_id, "n123");
        assert_eq!(result.name, "alice");
    }

    #[test]
    fn prompt_result_deserialize_optional_stop_reason() {
        let json = json!({
            "stopReason": "end_turn",
        });
        let result: PromptResult = serde_json::from_value(json).unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));

        let no_reason: PromptResult = serde_json::from_value(json!({})).unwrap();
        assert_eq!(no_reason.stop_reason, None);
    }

    #[test]
    fn dm_message_roundtrip() {
        let msg = DmMessage {
            role: "assistant".into(),
            content: "hello".into(),
            timestamp: 1710000000,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        assert_eq!(json["content"], "hello");

        let back: DmMessage = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn dm_state_roundtrip() {
        let state = DmState {
            node_id: "n1".into(),
            node_name: "alice".into(),
            messages: vec![
                DmMessage {
                    role: "user".into(),
                    content: "hi".into(),
                    timestamp: 1710000001,
                },
                DmMessage {
                    role: "assistant".into(),
                    content: "hello".into(),
                    timestamp: 1710000002,
                },
            ],
            streaming: Some("partial".into()),
            is_responding: true,
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["node_id"], "n1");
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["is_responding"], true);

        let back: DmState = serde_json::from_value(json).unwrap();
        assert_eq!(back, state);
    }
}
