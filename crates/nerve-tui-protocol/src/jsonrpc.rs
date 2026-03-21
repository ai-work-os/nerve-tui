use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A JSON-RPC 2.0 message (request, response, or notification).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcMessage {
    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    pub fn is_response(&self) -> bool {
        self.id.is_some() && (self.result.is_some() || self.error.is_some())
    }

    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }
}

pub fn encode_request(method: &str, params: Value) -> (u64, String) {
    let id = next_id();
    let msg = RpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(Value::Number(id.into())),
        method: Some(method.into()),
        params: Some(params),
        result: None,
        error: None,
    };
    (id, serde_json::to_string(&msg).unwrap())
}

pub fn encode_notification(method: &str, params: Value) -> String {
    let msg = RpcMessage {
        jsonrpc: "2.0".into(),
        id: None,
        method: Some(method.into()),
        params: Some(params),
        result: None,
        error: None,
    };
    serde_json::to_string(&msg).unwrap()
}

pub fn decode(text: &str) -> Result<RpcMessage, serde_json::Error> {
    serde_json::from_str(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_request_has_correct_format() {
        let (id, text) = encode_request("node.register", json!({"name": "tui"}));
        let msg: RpcMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(msg.jsonrpc, "2.0");
        assert_eq!(msg.method.as_deref(), Some("node.register"));
        assert_eq!(msg.id, Some(Value::Number(id.into())));
        assert!(msg.params.is_some());
        assert!(msg.result.is_none());
        assert!(msg.error.is_none());
    }

    #[test]
    fn encode_notification_has_no_id() {
        let text = encode_notification("channel.message", json!({"channelId": "ch1"}));
        let msg: RpcMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(msg.jsonrpc, "2.0");
        assert!(msg.id.is_none());
        assert_eq!(msg.method.as_deref(), Some("channel.message"));
    }

    #[test]
    fn decode_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"node.list","params":{}}"#;
        let msg = decode(json).unwrap();
        assert!(msg.is_request());
        assert!(!msg.is_response());
        assert!(!msg.is_notification());
    }

    #[test]
    fn decode_response_with_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let msg = decode(json).unwrap();
        assert!(msg.is_response());
        assert!(!msg.is_request());
        assert!(!msg.is_notification());
        assert!(msg.error.is_none());
    }

    #[test]
    fn decode_response_with_error() {
        let json = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"not found"}}"#;
        let msg = decode(json).unwrap();
        assert!(msg.is_response());
        let err = msg.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "not found");
    }

    #[test]
    fn decode_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"node.update","params":{"nodeId":"n1","name":"alice"}}"#;
        let msg = decode(json).unwrap();
        assert!(msg.is_notification());
        assert!(!msg.is_request());
        assert!(!msg.is_response());
    }

    #[test]
    fn next_id_increments() {
        let a = next_id();
        let b = next_id();
        assert!(b > a);
    }

    #[test]
    fn roundtrip_request() {
        let (id, text) = encode_request("test.method", json!({"key": "value"}));
        let decoded = decode(&text).unwrap();
        assert!(decoded.is_request());
        assert_eq!(decoded.id, Some(Value::Number(id.into())));
        assert_eq!(
            decoded.params.unwrap().get("key").unwrap().as_str(),
            Some("value")
        );
    }

    #[test]
    fn decode_invalid_json_fails() {
        assert!(decode("not json").is_err());
        assert!(decode("").is_err());
    }
}
