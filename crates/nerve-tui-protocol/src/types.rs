use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;
use tracing::{debug, warn};

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
    pub model: Option<String>,
    pub activity: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub last_active_at: f64,
    pub usage: Option<NodeUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeUsage {
    pub token_used: f64,
    pub token_size: f64,
    pub cost: f64,
    pub last_updated: f64,
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
    #[serde(alias = "channelId")]
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

// --- Render Model (Phase 1: 消息模型层) ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

/// A content block within a message — the core rendering abstraction.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        started_at: Option<Instant>,
        finished_at: Option<Instant>,
    },
    ToolCall {
        id: String,
        name: String,
        input: String,
        status: ToolStatus,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
    Error {
        message: String,
    },
}

impl ContentBlock {
    pub fn kind(&self) -> &'static str {
        match self {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Thinking { .. } => "thinking",
            ContentBlock::ToolCall { .. } => "tool_call",
            ContentBlock::ToolResult { .. } => "tool_result",
            ContentBlock::Error { .. } => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageMeta {
    pub timestamp: u64,
    pub model_id: Option<String>,
    /// True while streaming (not yet finalized).
    pub partial: bool,
}

/// A structured message composed of content blocks.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    pub meta: MessageMeta,
}

fn truncate_json(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.len() > max {
        format!("{}…({} bytes)", &s[..max], s.len())
    } else {
        s
    }
}

impl Message {
    pub fn new(id: String, role: Role, timestamp: u64) -> Self {
        Self {
            id,
            role,
            blocks: Vec::new(),
            meta: MessageMeta {
                timestamp,
                model_id: None,
                partial: true,
            },
        }
    }

    /// Close any open thinking block (set finished_at).
    /// Called when a non-thinking event arrives, since claude-agent-acp
    /// does NOT send agent_thought_end — thinking ends implicitly when
    /// the next text/tool event arrives.
    fn close_open_thinking(&mut self) {
        for block in self.blocks.iter_mut().rev() {
            if let ContentBlock::Thinking { finished_at, .. } = block {
                if finished_at.is_none() {
                    *finished_at = Some(Instant::now());
                    debug!(message_id = %self.id, "thinking block auto-closed by subsequent event");
                }
                break;
            }
        }
    }

    /// Parse a persisted content string back into ContentBlocks.
    ///
    /// The server stores messages as plain strings. This function detects
    /// tool_call JSON, tool_result JSON, and plain text, returning them
    /// as structured blocks so they can be rendered via block_renderer.
    pub fn content_to_blocks(content: &str) -> Vec<ContentBlock> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }

        // Try tool_call: JSON object with "name" + ("arguments" or "input")
        if trimmed.starts_with('{') {
            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                if let Some(obj) = val.as_object() {
                    // Tool call detection
                    if obj.contains_key("name")
                        && (obj.contains_key("arguments") || obj.contains_key("input"))
                    {
                        let name = obj
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let id = obj
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input_val = obj
                            .get("arguments")
                            .or_else(|| obj.get("input"));
                        let input = match input_val {
                            Some(v) if v.is_string() => {
                                v.as_str().unwrap_or("").to_string()
                            }
                            Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                            None => String::new(),
                        };
                        debug!(tool_name = %name, "content_to_blocks: detected tool_call");
                        return vec![ContentBlock::ToolCall {
                            id,
                            name,
                            input,
                            status: ToolStatus::Completed,
                        }];
                    }

                    // Tool result detection
                    let is_result = obj
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map_or(false, |t| t == "tool_result")
                        || obj.contains_key("tool_use_id");
                    if is_result {
                        let tool_call_id = obj
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let result_content = obj
                            .get("content")
                            .and_then(|v| v.as_str())
                            .or_else(|| obj.get("output").and_then(|v| v.as_str()))
                            .unwrap_or("")
                            .to_string();
                        let is_error = obj
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        debug!(tool_call_id = %tool_call_id, is_error, "content_to_blocks: detected tool_result");
                        return vec![ContentBlock::ToolResult {
                            tool_call_id,
                            content: result_content,
                            is_error,
                        }];
                    }
                }
            }
        }

        // Default: plain text
        debug!(len = trimmed.len(), "content_to_blocks: plain text");
        vec![ContentBlock::Text {
            text: content.to_string(),
        }]
    }

    /// Apply an ACP streaming event to this message.
    /// Returns true if the message was modified.
    pub fn apply_acp_event(&mut self, kind: &str, update: &Value) -> bool {
        debug!(message_id = %self.id, event_kind = kind, raw = %truncate_json(update, 500), "ACP event received");
        match kind {
            "agent_message_start" => {
                debug!(message_id = %self.id, "agent_message_start: new turn");
                self.meta.partial = true;
                true
            }
            "agent_message_chunk" => {
                self.close_open_thinking();
                let text = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if text.is_empty() {
                    return false;
                }
                // Append to last block only if it's Text; otherwise create new Text block
                if let Some(ContentBlock::Text { text: ref mut buf }) = self.blocks.last_mut() {
                    buf.push_str(text);
                    debug!(message_id = %self.id, chunk_len = text.len(), buf_len = buf.len(), "text chunk appended");
                } else {
                    debug!(message_id = %self.id, chunk_len = text.len(), "new text block created");
                    self.blocks.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
                true
            }
            "agent_thought_chunk" => {
                let text = update
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .or_else(|| update.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if text.is_empty() {
                    return false;
                }
                // Append to last block only if it's Thinking; otherwise create new
                if let Some(ContentBlock::Thinking { text: ref mut buf, started_at, .. }) = self.blocks.last_mut() {
                    buf.push_str(text);
                    if started_at.is_none() {
                        *started_at = Some(Instant::now());
                    }
                    debug!(message_id = %self.id, chunk_len = text.len(), "thought chunk appended");
                } else {
                    debug!(message_id = %self.id, chunk_len = text.len(), "new thinking block created");
                    self.blocks.push(ContentBlock::Thinking {
                        text: text.to_string(),
                        started_at: Some(Instant::now()),
                        finished_at: None,
                    });
                }
                true
            }
            "agent_thought_end" => {
                // Mark the last thinking block as finished
                if let Some(ContentBlock::Thinking { finished_at, .. }) = self.blocks.iter_mut().rev().find(|b| matches!(b, ContentBlock::Thinking { .. })) {
                    *finished_at = Some(Instant::now());
                    debug!(message_id = %self.id, "thinking block finished");
                } else {
                    warn!(message_id = %self.id, "agent_thought_end but no thinking block found");
                }
                true
            }
            "tool_call" => {
                self.close_open_thinking();
                // ACP sends tool_call as flat structure:
                //   { sessionUpdate: "tool_call", toolCallId: "...", title: "...", kind: "...",
                //     _meta: { claudeCode: { toolName: "Read" } }, rawInput: "...", content: [...] }
                // Also support legacy nested format: { toolCall: { id, name, input } }
                let tc = update
                    .get("toolCall")
                    .or_else(|| update.get("tool_call"));
                let (id, name, input) = if let Some(tc) = tc {
                    // Legacy nested format
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                    let input = tc.get("input").map(|v| {
                        if v.is_string() { v.as_str().unwrap().to_string() }
                        else { serde_json::to_string_pretty(v).unwrap_or_default() }
                    }).unwrap_or_default();
                    (id, name, input)
                } else if update.get("toolCallId").is_some() {
                    // Flat ACP format (claude-agent-acp)
                    let id = update.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = update
                        .pointer("/_meta/claudeCode/toolName")
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| update.get("title").and_then(|v| v.as_str()).unwrap_or("unknown"))
                        .to_string();
                    let input = update.get("rawInput").map(|v| {
                        if v.is_string() { v.as_str().unwrap().to_string() }
                        else { serde_json::to_string_pretty(v).unwrap_or_default() }
                    }).unwrap_or_else(|| {
                        update.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string()
                    });
                    (id, name, input)
                } else {
                    warn!(message_id = %self.id, "tool_call event missing toolCall/toolCallId field");
                    return false;
                };
                debug!(message_id = %self.id, tool_id = %id, tool_name = %name, "tool_call block created");
                self.blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    input,
                    status: ToolStatus::Pending,
                });
                true
            }
            "tool_call_update" => {
                // ACP flat format: { sessionUpdate: "tool_call_update", toolCallId: "...", status: "completed", content: [...] }
                // Legacy nested: { toolCallUpdate: { id, status, content } }
                let tcu = update
                    .get("toolCallUpdate")
                    .or_else(|| update.get("tool_call_update"));
                let (tc_id, new_status, result_content) = if let Some(tcu) = tcu {
                    // Legacy nested
                    let id = tcu.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let status = match tcu.get("status").and_then(|v| v.as_str()) {
                        Some("completed") => Some(ToolStatus::Completed),
                        Some("failed") => Some(ToolStatus::Failed),
                        Some("running") | Some("in_progress") => Some(ToolStatus::Running),
                        Some("pending") => Some(ToolStatus::Pending),
                        _ => None,
                    };
                    let content = tcu.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    (id.to_string(), status, content)
                } else if update.get("toolCallId").is_some() {
                    // Flat ACP format
                    let id = update.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let status = match update.get("status").and_then(|v| v.as_str()) {
                        Some("completed") => Some(ToolStatus::Completed),
                        Some("failed") => Some(ToolStatus::Failed),
                        Some("running") | Some("in_progress") => Some(ToolStatus::Running),
                        Some("pending") => Some(ToolStatus::Pending),
                        _ => None,
                    };
                    // ACP content is array of { type: "content", content: { type: "text", text: "..." } }
                    let content = update.get("content")
                        .and_then(|v| {
                            if v.is_string() {
                                v.as_str().map(|s| s.to_string())
                            } else if let Some(arr) = v.as_array() {
                                let texts: Vec<String> = arr.iter().filter_map(|item| {
                                    item.get("content")
                                        .and_then(|c| c.get("text"))
                                        .and_then(|t| t.as_str())
                                        .map(|s| s.to_string())
                                }).collect();
                                if texts.is_empty() { None } else { Some(texts.join("\n")) }
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    (id, status, content)
                } else {
                    return false;
                };

                // Find matching ToolCall by id, or fall back to last ToolCall
                let found = if !tc_id.is_empty() {
                    self.blocks.iter_mut().rev().find(|b| matches!(b, ContentBlock::ToolCall { id, .. } if id == &tc_id))
                } else {
                    self.blocks.iter_mut().rev().find(|b| matches!(b, ContentBlock::ToolCall { .. }))
                };

                match found {
                    Some(ContentBlock::ToolCall { status, id, .. }) => {
                        debug!(message_id = %self.id, tool_id = %id, ?new_status, "tool_call status updated");
                        if let Some(s) = new_status {
                            *status = s;
                        }
                    }
                    _ => {
                        warn!(message_id = %self.id, tool_call_id = %tc_id, "tool_call_update: no matching ToolCall block");
                        return false;
                    }
                }

                // Add ToolResult block if there's result content
                if !result_content.is_empty() {
                    let is_error = new_status == Some(ToolStatus::Failed);
                    self.blocks.push(ContentBlock::ToolResult {
                        tool_call_id: tc_id,
                        content: result_content,
                        is_error,
                    });
                }
                true
            }
            "agent_message_end" | "stopReason" => {
                debug!(message_id = %self.id, blocks = self.blocks.len(), "message finalized");
                self.meta.partial = false;
                true
            }
            _ => {
                debug!(message_id = %self.id, event_kind = kind, "unknown ACP event ignored");
                false
            }
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
    fn node_info_deserialize_with_model() {
        let json = json!({
            "id": "n3",
            "name": "claude-agent",
            "status": "idle",
            "adapter": "claude",
            "model": "opus[1m]",
            "channels": [],
        });
        let node: NodeInfo = serde_json::from_value(json).unwrap();
        assert_eq!(node.id, "n3");
        assert_eq!(node.model, Some("opus[1m]".to_string()));
    }

    #[test]
    fn node_info_deserialize_without_model() {
        let json = json!({
            "id": "n4",
            "name": "mock-agent",
            "status": "idle",
            "channels": [],
        });
        let node: NodeInfo = serde_json::from_value(json).unwrap();
        assert!(node.model.is_none());
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
            model: None,
            activity: None,
            channels: vec![],
            created_at: 0.0,
            last_active_at: 0.0,
            usage: None,
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

    // --- ContentBlock / Message tests ---

    fn make_msg() -> Message {
        Message::new("m1".into(), Role::Assistant, 1710000000)
    }

    #[test]
    fn message_new_is_partial() {
        let msg = make_msg();
        assert!(msg.meta.partial);
        assert!(msg.blocks.is_empty());
        assert_eq!(msg.role, Role::Assistant);
    }

    #[test]
    fn agent_message_chunk_creates_text_block() {
        let mut msg = make_msg();
        let applied = msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "Hello " }
        }));
        assert!(applied);
        assert_eq!(msg.blocks.len(), 1);
        assert!(matches!(&msg.blocks[0], ContentBlock::Text { text } if text == "Hello "));
    }

    #[test]
    fn agent_message_chunk_appends_to_last_text() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "Hello " }
        }));
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "world!" }
        }));
        assert_eq!(msg.blocks.len(), 1);
        assert!(matches!(&msg.blocks[0], ContentBlock::Text { text } if text == "Hello world!"));
    }

    #[test]
    fn agent_message_chunk_empty_text_ignored() {
        let mut msg = make_msg();
        let applied = msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "" }
        }));
        assert!(!applied);
        assert!(msg.blocks.is_empty());
    }

    #[test]
    fn agent_thought_chunk_creates_thinking_block() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "Let me think..." }
        }));
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::Thinking { text, started_at, finished_at } => {
                assert_eq!(text, "Let me think...");
                assert!(started_at.is_some());
                assert!(finished_at.is_none());
            }
            _ => panic!("expected Thinking block"),
        }
    }

    #[test]
    fn agent_thought_chunk_appends_to_last_thinking() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "Step 1. " }
        }));
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "Step 2." }
        }));
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::Thinking { text, .. } => {
                assert_eq!(text, "Step 1. Step 2.");
            }
            _ => panic!("expected Thinking block"),
        }
    }

    #[test]
    fn agent_thought_end_sets_finished_at() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "thinking..." }
        }));
        msg.apply_acp_event("agent_thought_end", &json!({}));
        match &msg.blocks[0] {
            ContentBlock::Thinking { finished_at, .. } => {
                assert!(finished_at.is_some());
            }
            _ => panic!("expected Thinking block"),
        }
    }

    #[test]
    fn tool_call_creates_tool_call_block() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": {
                "id": "tc1",
                "name": "bash",
                "input": { "command": "ls" }
            }
        }));
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, name, status, .. } => {
                assert_eq!(id, "tc1");
                assert_eq!(name, "bash");
                assert_eq!(*status, ToolStatus::Pending);
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_changes_status() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "bash", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": { "id": "tc1", "status": "completed" }
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => {
                assert_eq!(*status, ToolStatus::Completed);
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_with_content_adds_tool_result() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "bash", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": {
                "id": "tc1",
                "status": "completed",
                "content": "file1.txt\nfile2.txt"
            }
        }));
        assert_eq!(msg.blocks.len(), 2);
        match &msg.blocks[1] {
            ContentBlock::ToolResult { tool_call_id, content, is_error } => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(content, "file1.txt\nfile2.txt");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn tool_call_update_failed_marks_error() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "bash", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": {
                "id": "tc1",
                "status": "failed",
                "content": "command not found"
            }
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => assert_eq!(*status, ToolStatus::Failed),
            _ => panic!("expected ToolCall block"),
        }
        match &msg.blocks[1] {
            ContentBlock::ToolResult { is_error, .. } => assert!(is_error),
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn agent_message_end_clears_partial() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "done" }
        }));
        assert!(msg.meta.partial);
        msg.apply_acp_event("agent_message_end", &json!({}));
        assert!(!msg.meta.partial);
    }

    #[test]
    fn stop_reason_clears_partial() {
        let mut msg = make_msg();
        msg.apply_acp_event("stopReason", &json!({}));
        assert!(!msg.meta.partial);
    }

    #[test]
    fn unknown_event_returns_false() {
        let mut msg = make_msg();
        assert!(!msg.apply_acp_event("unknown_event", &json!({})));
        assert!(msg.blocks.is_empty());
    }

    #[test]
    fn tool_call_flat_acp_format() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_abc123",
            "rawInput": { "command": "ls -la" },
            "status": "pending",
            "title": "ls -la",
            "kind": "execute",
            "_meta": { "claudeCode": { "toolName": "Bash" } }
        }));
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, name, input, status } => {
                assert_eq!(id, "toolu_abc123");
                assert_eq!(name, "Bash");
                assert!(input.contains("ls -la"));
                assert_eq!(*status, ToolStatus::Pending);
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_flat_acp_format() {
        let mut msg = make_msg();
        // First create a tool_call in flat format
        msg.apply_acp_event("tool_call", &json!({
            "toolCallId": "toolu_abc123",
            "_meta": { "claudeCode": { "toolName": "Bash" } },
            "title": "ls",
            "kind": "execute"
        }));
        // Then update it in flat format
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallId": "toolu_abc123",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": "file1.txt\nfile2.txt" } }
            ]
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => assert_eq!(*status, ToolStatus::Completed),
            _ => panic!("expected ToolCall block"),
        }
        assert_eq!(msg.blocks.len(), 2);
        match &msg.blocks[1] {
            ContentBlock::ToolResult { tool_call_id, content, is_error } => {
                assert_eq!(tool_call_id, "toolu_abc123");
                assert_eq!(content, "file1.txt\nfile2.txt");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    /// Full streaming sequence: thinking → text → tool_call → tool_result → text → end
    #[test]
    fn full_acp_streaming_sequence() {
        let mut msg = make_msg();

        // 1. Agent starts
        msg.apply_acp_event("agent_message_start", &json!({}));
        assert!(msg.meta.partial);

        // 2. Thinking chunks
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "I need to " }
        }));
        msg.apply_acp_event("agent_thought_chunk", &json!({
            "content": { "text": "check the file." }
        }));
        msg.apply_acp_event("agent_thought_end", &json!({}));

        // 3. Text chunk
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "Let me check. " }
        }));

        // 4. Tool call
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "Read", "input": { "path": "/tmp/test" } }
        }));

        // 5. Tool result
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": { "id": "tc1", "status": "completed", "content": "file contents" }
        }));

        // 6. More text
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "The file contains..." }
        }));

        // 7. End
        msg.apply_acp_event("agent_message_end", &json!({}));

        // Verify block sequence: thinking, text, tool_call, tool_result, text (new after tool)
        assert_eq!(msg.blocks.len(), 5);
        assert_eq!(msg.blocks[0].kind(), "thinking");
        assert_eq!(msg.blocks[1].kind(), "text");
        assert_eq!(msg.blocks[2].kind(), "tool_call");
        assert_eq!(msg.blocks[3].kind(), "tool_result");
        assert_eq!(msg.blocks[4].kind(), "text");
        match &msg.blocks[0] {
            ContentBlock::Thinking { text, started_at, finished_at } => {
                assert_eq!(text, "I need to check the file.");
                assert!(started_at.is_some());
                assert!(finished_at.is_some());
            }
            _ => panic!("expected Thinking block"),
        }
        match &msg.blocks[1] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "Let me check. ");
            }
            _ => panic!("expected Text block"),
        }
        match &msg.blocks[4] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "The file contains...");
            }
            _ => panic!("expected Text block"),
        }
        assert!(!msg.meta.partial);
    }

    /// Text chunks after a tool_call create a new Text block (not appended to pre-tool text).
    #[test]
    fn text_after_tool_creates_new_text_block() {
        let mut msg = make_msg();
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "before " }
        }));
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "bash", "input": {} }
        }));
        msg.apply_acp_event("agent_message_chunk", &json!({
            "content": { "text": "after" }
        }));
        // Text + ToolCall + Text (new block after tool)
        assert_eq!(msg.blocks.len(), 3);
        match &msg.blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "before "),
            _ => panic!("expected Text"),
        }
        match &msg.blocks[2] {
            ContentBlock::Text { text } => assert_eq!(text, "after"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn tool_call_snake_case_key_works() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call_update", &json!({
            "tool_call_update": { "id": "tc1", "status": "running" }
        }));
        // No matching tool_call block, so returns false
        assert!(msg.blocks.is_empty());
    }

    #[test]
    fn role_serde_roundtrip() {
        let json = serde_json::to_string(&Role::Assistant).unwrap();
        assert_eq!(json, "\"assistant\"");
        let back: Role = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Role::Assistant);
    }

    #[test]
    fn tool_status_serde_roundtrip() {
        let json = serde_json::to_string(&ToolStatus::Completed).unwrap();
        assert_eq!(json, "\"completed\"");
        let back: ToolStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ToolStatus::Completed);
    }

    // --- ACP spec alignment tests ---

    #[test]
    fn tool_call_update_in_progress_status() {
        // ACP spec uses "in_progress" not "running"
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCallId": "tc1",
            "_meta": { "claudeCode": { "toolName": "Bash" } },
            "title": "ls",
            "kind": "execute"
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallId": "tc1",
            "status": "in_progress"
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => assert_eq!(*status, ToolStatus::Running),
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_in_progress_legacy() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "bash", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": { "id": "tc1", "status": "in_progress" }
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => assert_eq!(*status, ToolStatus::Running),
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_pending_status() {
        // ACP spec "pending" status
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCallId": "tc1",
            "_meta": { "claudeCode": { "toolName": "Read" } },
            "title": "read file",
            "kind": "read"
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallId": "tc1",
            "status": "pending"
        }));
        match &msg.blocks[0] {
            ContentBlock::ToolCall { status, .. } => assert_eq!(*status, ToolStatus::Pending),
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_flat_with_locations() {
        // ACP spec: tool_call can include locations field
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_001",
            "title": "Reading configuration file",
            "kind": "read",
            "status": "pending",
            "content": [],
            "locations": [{"path": "/foo/bar.rs"}],
            "rawInput": {"file_path": "/foo/bar.rs"},
            "_meta": { "claudeCode": { "toolName": "Read" } }
        }));
        assert_eq!(msg.blocks.len(), 1);
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, name, input, status } => {
                assert_eq!(id, "call_001");
                assert_eq!(name, "Read");
                assert!(input.contains("bar.rs"));
                assert_eq!(*status, ToolStatus::Pending);
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn tool_call_update_with_acp_content_array() {
        // ACP spec: content is array of ToolCallContent items
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCallId": "call_001",
            "_meta": { "claudeCode": { "toolName": "Bash" } },
            "title": "ls"
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallId": "call_001",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": "line1" } },
                { "type": "content", "content": { "type": "text", "text": "line2" } }
            ]
        }));
        match &msg.blocks[1] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "line1\nline2");
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn content_block_kind() {
        assert_eq!(ContentBlock::Text { text: String::new() }.kind(), "text");
        assert_eq!(ContentBlock::Thinking {
            text: String::new(),
            started_at: None,
            finished_at: None,
        }.kind(), "thinking");
        assert_eq!(ContentBlock::Error { message: String::new() }.kind(), "error");
    }

    // --- content_to_blocks tests ---

    #[test]
    fn content_to_blocks_empty_string() {
        let blocks = Message::content_to_blocks("");
        assert!(blocks.is_empty());
    }

    #[test]
    fn content_to_blocks_whitespace_only() {
        let blocks = Message::content_to_blocks("   \n  ");
        assert!(blocks.is_empty());
    }

    #[test]
    fn content_to_blocks_plain_text() {
        let blocks = Message::content_to_blocks("Hello world");
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_to_blocks_markdown_text() {
        let content = "## Title\n\nSome **bold** text with `code`";
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, content),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_call_with_arguments() {
        let content = r#"{"name": "Read", "arguments": {"file_path": "/tmp/test.rs"}}"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolCall { name, input, status, .. } => {
                assert_eq!(name, "Read");
                assert!(input.contains("/tmp/test.rs"));
                assert_eq!(*status, ToolStatus::Completed);
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_call_with_input() {
        let content = r#"{"name": "Bash", "input": {"command": "ls -la"}}"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolCall { name, input, .. } => {
                assert_eq!(name, "Bash");
                assert!(input.contains("ls -la"));
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_call_with_id() {
        let content = r#"{"id": "tc1", "name": "Edit", "arguments": {}}"#;
        let blocks = Message::content_to_blocks(content);
        match &blocks[0] {
            ContentBlock::ToolCall { id, name, .. } => {
                assert_eq!(id, "tc1");
                assert_eq!(name, "Edit");
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_call_string_input() {
        let content = r#"{"name": "Write", "arguments": "raw string arg"}"#;
        let blocks = Message::content_to_blocks(content);
        match &blocks[0] {
            ContentBlock::ToolCall { input, .. } => {
                assert_eq!(input, "raw string arg");
            }
            _ => panic!("expected ToolCall block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_result_by_type() {
        let content = r#"{"type": "tool_result", "content": "file1.txt\nfile2.txt"}"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolResult { content, is_error, .. } => {
                assert_eq!(content, "file1.txt\nfile2.txt");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_result_by_tool_use_id() {
        let content = r#"{"tool_use_id": "tc1", "content": "ok"}"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolResult { tool_call_id, content, .. } => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(content, "ok");
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_result_error() {
        let content = r#"{"type": "tool_result", "content": "not found", "is_error": true}"#;
        let blocks = Message::content_to_blocks(content);
        match &blocks[0] {
            ContentBlock::ToolResult { is_error, content, .. } => {
                assert!(is_error);
                assert_eq!(content, "not found");
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn content_to_blocks_tool_result_with_output_field() {
        let content = r#"{"type": "tool_result", "output": "some output"}"#;
        let blocks = Message::content_to_blocks(content);
        match &blocks[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "some output");
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn content_to_blocks_json_without_name_is_text() {
        // A JSON object that doesn't match tool_call or tool_result patterns
        let content = r#"{"key": "value", "other": 42}"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, content),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_to_blocks_invalid_json_is_text() {
        let content = r#"{broken json"#;
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, content),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_to_blocks_preserves_original_text() {
        // Ensure leading/trailing whitespace in the original is preserved in Text block
        let content = "  some text with spaces  ";
        let blocks = Message::content_to_blocks(content);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, content),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_to_blocks_multiline_text() {
        let content = "line 1\nline 2\nline 3";
        let blocks = Message::content_to_blocks(content);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, content),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn multiple_tool_calls_in_sequence() {
        let mut msg = make_msg();
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc1", "name": "Read", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": { "id": "tc1", "status": "completed", "content": "ok" }
        }));
        msg.apply_acp_event("tool_call", &json!({
            "toolCall": { "id": "tc2", "name": "Edit", "input": {} }
        }));
        msg.apply_acp_event("tool_call_update", &json!({
            "toolCallUpdate": { "id": "tc2", "status": "failed", "content": "error" }
        }));

        assert_eq!(msg.blocks.len(), 4); // ToolCall + ToolResult + ToolCall + ToolResult
        match &msg.blocks[0] {
            ContentBlock::ToolCall { id, status, .. } => {
                assert_eq!(id, "tc1");
                assert_eq!(*status, ToolStatus::Completed);
            }
            _ => panic!("expected ToolCall"),
        }
        match &msg.blocks[2] {
            ContentBlock::ToolCall { id, status, .. } => {
                assert_eq!(id, "tc2");
                assert_eq!(*status, ToolStatus::Failed);
            }
            _ => panic!("expected ToolCall"),
        }
        match &msg.blocks[3] {
            ContentBlock::ToolResult { is_error, .. } => assert!(is_error),
            _ => panic!("expected ToolResult"),
        }
    }
}
