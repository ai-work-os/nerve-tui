use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::transport::Transport;

/// Records each (method, params) call and returns a preset response or error.
#[derive(Clone)]
pub struct MockTransport {
    name: String,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    response: Arc<Mutex<Value>>,
    error: Arc<Mutex<Option<String>>>,
}

impl MockTransport {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(Value::Null)),
            error: Arc::new(Mutex::new(None)),
        }
    }

    pub fn set_response(&self, v: Value) {
        *self.response.lock().unwrap() = v;
        *self.error.lock().unwrap() = None;
    }

    pub fn set_error(&self, msg: &str) {
        *self.error.lock().unwrap() = Some(msg.to_string());
    }

    pub fn last_call(&self) -> (String, Value) {
        self.calls.lock().unwrap().last().cloned().unwrap()
    }

    pub fn call_count(&self) -> usize {
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
