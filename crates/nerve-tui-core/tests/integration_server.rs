#![cfg(feature = "integration")]

//! Integration tests: NerveClient ↔ real nerve server.
//!
//! Run with: cargo test -p nerve-tui-core --features integration -- --test-threads=1

use nerve_tui_core::NerveClient;
use nerve_tui_protocol::NerveEvent;
use serde_json::json;
use std::net::TcpStream;
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

const TEST_PORT: u16 = 4801;
const WS_URL: &str = "ws://127.0.0.1:4801";
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

// --- Server lifecycle (module-level singleton) ---

struct TestServer {
    process: Child,
    data_dir: std::path::PathBuf,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

static SERVER: OnceLock<TestServer> = OnceLock::new();

fn ensure_server() {
    SERVER.get_or_init(|| {
        let data_dir = std::env::temp_dir().join(format!("nerve-test-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).expect("create temp data dir");

        // Find nerve/ directory
        let nerve_dir = find_nerve_dir();

        let process = Command::new("npx")
            .args([
                "tsx",
                "src/cli.ts",
                "serve",
                "--port",
                &TEST_PORT.to_string(),
                "--data",
                data_dir.to_str().unwrap(),
                "--no-guardian",
            ])
            .current_dir(&nerve_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to start nerve server");

        // Poll TCP until server accepts connections
        for i in 0..30 {
            if TcpStream::connect(("127.0.0.1", TEST_PORT)).is_ok() {
                // Give server a moment to fully initialize
                std::thread::sleep(Duration::from_millis(500));
                return TestServer { process, data_dir };
            }
            if i == 29 {
                panic!("nerve server failed to start after 30 retries");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        unreachable!()
    });
}

fn find_nerve_dir() -> std::path::PathBuf {
    let candidates = [
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../nerve"),
        dirs::home_dir()
            .unwrap_or_default()
            .join("work/worktree/ai-work-os/nerve"),
        dirs::home_dir()
            .unwrap_or_default()
            .join("work/ai-work-os/nerve"),
    ];
    for c in &candidates {
        if c.join("src/cli.ts").exists() {
            return c.canonicalize().unwrap();
        }
    }
    panic!(
        "Cannot find nerve/ directory. Tried: {:?}",
        candidates.iter().map(|c| c.display().to_string()).collect::<Vec<_>>()
    );
}

// --- Helpers ---

async fn connect(name: &str) -> (NerveClient, mpsc::UnboundedReceiver<NerveEvent>) {
    ensure_server();
    NerveClient::connect(WS_URL, name)
        .await
        .expect("connect failed")
}

/// Wait for a specific event, returning it.
async fn wait_event<F>(rx: &mut mpsc::UnboundedReceiver<NerveEvent>, pred: F) -> NerveEvent
where
    F: Fn(&NerveEvent) -> bool,
{
    timeout(EVENT_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(evt) if pred(&evt) => return evt,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

// --- Tests ---

#[tokio::test]
async fn t1_connect_and_register() {
    let (client, _rx) = connect("int-test-1").await;
    assert!(
        client.node_id.is_some(),
        "node_id should be set after register"
    );
    assert!(
        !client.node_id.as_ref().unwrap().is_empty(),
        "node_id should not be empty"
    );
}

#[tokio::test]
async fn t2_list_nodes_contains_self() {
    let (client, _rx) = connect("int-test-2").await;
    let node_id = client.node_id.as_ref().unwrap().clone();

    let nodes = client.node_list(None).await.unwrap();
    assert!(
        nodes.iter().any(|n| n.id == node_id),
        "node list should contain self"
    );
}

#[tokio::test]
async fn t3_spawn_mock_program() {
    let (client, _rx) = connect("int-test-3").await;

    let result = client.node_spawn("mock-program", None, None).await.unwrap();
    assert!(!result.node_id.is_empty(), "spawn should return nodeId");

    // Verify it appears in node list
    let nodes = client.node_list(None).await.unwrap();
    assert!(
        nodes.iter().any(|n| n.id == result.node_id),
        "spawned node should be in node list"
    );

    // Cleanup
    let _ = client.node_stop(&result.node_id).await;
}

#[tokio::test]
async fn t4_subscribe_receives_status_changed() {
    let (client, mut rx) = connect("int-test-4").await;

    let result = client.node_spawn("mock-program", None, None).await.unwrap();
    client.node_subscribe(&result.node_id).await.unwrap();

    let evt = wait_event(&mut rx, |e| {
        matches!(e, NerveEvent::NodeStatusChanged { node_id, activity, .. }
            if node_id == &result.node_id && activity.as_deref() == Some("ready"))
    })
    .await;

    assert!(
        matches!(evt, NerveEvent::NodeStatusChanged { .. }),
        "should receive NodeStatusChanged with activity=ready"
    );

    // Cleanup
    let _ = client.node_stop(&result.node_id).await;
}

#[tokio::test]
async fn t5_node_message_echo() {
    let (client, mut rx) = connect("int-test-5").await;

    let result = client.node_spawn("mock-program", None, None).await.unwrap();
    client.node_subscribe(&result.node_id).await.unwrap();

    // Wait for mock-program to be ready
    wait_event(&mut rx, |e| {
        matches!(e, NerveEvent::NodeStatusChanged { node_id, activity, .. }
            if node_id == &result.node_id && activity.as_deref() == Some("ready"))
    })
    .await;

    // Send a message to mock-program
    client
        .node_message(&result.node_id, "ping-test")
        .await
        .unwrap();

    // mock-program echoes back via node.log → we should see a NodeUpdate containing the echo
    let evt = wait_event(&mut rx, |e| {
        if let NerveEvent::NodeUpdate { node_id, detail, .. } = e {
            node_id == &result.node_id
                && detail.to_string().contains("ping-test")
        } else {
            false
        }
    })
    .await;

    assert!(
        matches!(evt, NerveEvent::NodeUpdate { .. }),
        "should receive NodeUpdate with echo content"
    );

    // Cleanup
    let _ = client.node_stop(&result.node_id).await;
}

#[tokio::test]
async fn t6_channel_create_join_post_history() {
    let (client, _rx) = connect("int-test-6").await;

    // Create channel
    let ch = client
        .channel_create(Some("int-test-ch"), None)
        .await
        .unwrap();
    assert!(!ch.id.is_empty(), "channel id should not be empty");

    // Join channel
    client.channel_join(&ch.id).await.unwrap();

    // Post messages
    let msg = client.channel_post(&ch.id, "hello-int").await.unwrap();
    assert_eq!(msg.content, "hello-int");

    client.channel_post(&ch.id, "second-msg").await.unwrap();

    // Fetch history
    let history = client.channel_history(&ch.id, None).await.unwrap();
    let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
    assert!(
        contents.contains(&"hello-int"),
        "history should contain hello-int"
    );
    assert!(
        contents.contains(&"second-msg"),
        "history should contain second-msg"
    );
}

#[tokio::test]
async fn t7_channel_leave() {
    let (client, _rx) = connect("int-test-7").await;

    let ch = client
        .channel_create(Some("int-test-leave"), None)
        .await
        .unwrap();
    client.channel_join(&ch.id).await.unwrap();

    // Leave channel via raw request (no convenience method)
    let result = client
        .request("channel.leave", json!({ "channelId": ch.id }))
        .await;
    assert!(result.is_ok(), "channel.leave should not error");
}

#[tokio::test]
async fn t8_stop_node_emits_stopped() {
    let (client, mut rx) = connect("int-test-8").await;

    let result = client.node_spawn("mock-program", None, None).await.unwrap();
    client.node_subscribe(&result.node_id).await.unwrap();

    // Wait for ready
    wait_event(&mut rx, |e| {
        matches!(e, NerveEvent::NodeStatusChanged { node_id, activity, .. }
            if node_id == &result.node_id && activity.as_deref() == Some("ready"))
    })
    .await;

    // Stop the node
    client.node_stop(&result.node_id).await.unwrap();

    // Should receive NodeStopped
    let evt = wait_event(&mut rx, |e| {
        matches!(e, NerveEvent::NodeStopped { node_id, .. } if node_id == &result.node_id)
    })
    .await;

    assert!(
        matches!(evt, NerveEvent::NodeStopped { .. }),
        "should receive NodeStopped"
    );
}

#[tokio::test]
async fn t9_stop_nonexistent_node() {
    let (client, _rx) = connect("int-test-9").await;

    // Stopping a nonexistent node should not panic/error at the transport level
    // Server may return an error response, but it shouldn't crash
    let result = client.node_stop("nonexistent-node-id-99999").await;
    // We accept both Ok and Err — the important thing is no panic
    let _ = result;
}

#[tokio::test]
async fn t10_node_list_contains_model_field() {
    let (client, _rx) = connect("int-test-10").await;

    // Spawn a mock-program node
    let result = client.node_spawn("mock-program", None, None).await.unwrap();
    assert!(!result.node_id.is_empty(), "spawn should return nodeId");

    // Fetch node list and verify the NodeInfo has model field
    let nodes = client.node_list(None).await.unwrap();
    let node = nodes.iter().find(|n| n.id == result.node_id);
    assert!(node.is_some(), "spawned node should be in node list");

    // model field should be accessible (may be None for mock-program, but field must exist on struct)
    let node = node.unwrap();
    // This line will fail to compile until NodeInfo has `model: Option<String>`
    let _model: &Option<String> = &node.model;

    // Cleanup
    let _ = client.node_stop(&result.node_id).await;
}
