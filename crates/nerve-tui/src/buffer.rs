use std::collections::HashMap;

/// Buffer 的唯一标识，按语义查找
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BufferId {
    Channel { channel_id: String },
    Dm { node_id: String },
    NodeLog { node_id: String },
}

/// 简化版 ChannelView 桩 — 只保留 messages + push + clear
/// Phase 2 时替换为真实 ChannelView（含 ContentBlock 等）
pub struct ChannelViewStub {
    pub messages: Vec<String>,
}

impl ChannelViewStub {
    pub fn new() -> Self {
        Self { messages: Vec::new() }
    }

    pub fn push(&mut self, msg: &str) {
        self.messages.push(msg.to_string());
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

/// 简化版 DmView 桩 — 只保留 messages + push + clear
/// Phase 2 时替换为真实 DmView（含 streaming pipeline）
pub struct DmViewStub {
    pub messages: Vec<String>,
}

impl DmViewStub {
    pub fn new() -> Self {
        Self { messages: Vec::new() }
    }

    pub fn push(&mut self, msg: &str) {
        self.messages.push(msg.to_string());
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn messages(&self) -> &[String] {
        &self.messages
    }
}

/// Buffer 内容，三种类型各自独立
/// Channel/Dm 使用 Stub 桩（Phase 2 替换为真实 View）
pub enum BufferContent {
    Channel(ChannelViewStub),
    Dm(DmViewStub),
    NodeLog { text: String, pending: bool },
}

/// Buffer 条目：id + 内容 + 版本号
pub struct BufferEntry {
    pub id: BufferId,
    pub content: BufferContent,
    pub content_version: u64,
}

impl BufferEntry {
    /// 创建新 buffer entry
    pub fn new(id: BufferId, content: BufferContent) -> Self {
        Self { id, content, content_version: 0 }
    }

    /// 内容变更时调用，递增版本号
    pub fn bump_version(&mut self) {
        self.content_version += 1;
    }

    /// 清空内容并重置 content_version 为 0
    pub fn clear(&mut self) {
        match &mut self.content {
            BufferContent::Channel(cv) => cv.clear(),
            BufferContent::Dm(dv) => dv.clear(),
            BufferContent::NodeLog { text, pending } => {
                text.clear();
                *pending = false;
            }
        }
        self.content_version = 0;
    }
}

/// Buffer 池：管理所有 buffer 实例
pub struct BufferPool {
    buffers: HashMap<BufferId, BufferEntry>,
}

impl BufferPool {
    pub fn new() -> Self {
        Self { buffers: HashMap::new() }
    }

    /// 获取已有 buffer 的不可变引用
    pub fn get(&self, id: &BufferId) -> Option<&BufferEntry> {
        self.buffers.get(id)
    }

    /// 获取已有 buffer 的可变引用
    pub fn get_mut(&mut self, id: &BufferId) -> Option<&mut BufferEntry> {
        self.buffers.get_mut(id)
    }

    /// 获取或创建 buffer，根据 BufferId variant 创建对应的 BufferContent
    pub fn get_or_create(&mut self, id: BufferId) -> &mut BufferEntry {
        self.buffers.entry(id.clone()).or_insert_with(|| {
            let content = match &id {
                BufferId::Channel { .. } => BufferContent::Channel(ChannelViewStub::new()),
                BufferId::Dm { .. } => BufferContent::Dm(DmViewStub::new()),
                BufferId::NodeLog { .. } => BufferContent::NodeLog { text: String::new(), pending: false },
            };
            BufferEntry::new(id, content)
        })
    }
}

/// 显示一个 buffer 的视口，持有滚动状态
#[derive(Debug, Clone)]
pub struct Window {
    pub buffer_id: BufferId,
    pub scroll_offset: u16,
    pub auto_scroll: bool,
    pub has_new_messages: bool,
    /// 上次渲染时看到的 content_version
    pub last_seen_version: u64,
}

impl Window {
    /// 创建新窗口，last_seen_version 初始化为 buffer 当前版本
    pub fn new(buffer_id: BufferId, current_content_version: u64) -> Self {
        Self {
            buffer_id,
            scroll_offset: 0,
            auto_scroll: true,
            has_new_messages: false,
            last_seen_version: current_content_version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: unsubscribe 调用测试在 Phase 2 App 集成测试中覆盖
    // Phase 1 只测 buffer/pool 数据结构层，不涉及 client mock

    // ========== 1. BufferId 相等性 ==========

    #[test]
    fn buffer_id_channel_eq() {
        let a = BufferId::Channel { channel_id: "ch1".into() };
        let b = BufferId::Channel { channel_id: "ch1".into() };
        assert_eq!(a, b);
    }

    #[test]
    fn buffer_id_channel_ne() {
        let a = BufferId::Channel { channel_id: "ch1".into() };
        let b = BufferId::Channel { channel_id: "ch2".into() };
        assert_ne!(a, b);
    }

    #[test]
    fn buffer_id_dm_eq() {
        let a = BufferId::Dm { node_id: "node1".into() };
        let b = BufferId::Dm { node_id: "node1".into() };
        assert_eq!(a, b);
    }

    #[test]
    fn buffer_id_node_log_eq() {
        let a = BufferId::NodeLog { node_id: "node1".into() };
        let b = BufferId::NodeLog { node_id: "node1".into() };
        assert_eq!(a, b);
    }

    #[test]
    fn buffer_id_different_variants_ne() {
        let ch = BufferId::Channel { channel_id: "x".into() };
        let dm = BufferId::Dm { node_id: "x".into() };
        let log = BufferId::NodeLog { node_id: "x".into() };
        assert_ne!(ch, dm);
        assert_ne!(dm, log);
        assert_ne!(ch, log);
    }

    #[test]
    fn buffer_id_hash_consistency() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        set.insert(id.clone());
        assert!(set.contains(&BufferId::Channel { channel_id: "ch1".into() }));
        assert!(!set.contains(&BufferId::Channel { channel_id: "ch2".into() }));
    }

    // ========== 2. BufferPool 基础操作 ==========

    #[test]
    fn pool_get_or_create_new_channel_buffer() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id.clone());
        assert_eq!(entry.id, id);
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::Channel(cv) => assert!(cv.messages().is_empty()),
            _ => panic!("expected Channel variant"),
        }
    }

    #[test]
    fn pool_get_or_create_new_dm_buffer() {
        let mut pool = BufferPool::new();
        let id = BufferId::Dm { node_id: "node1".into() };
        let entry = pool.get_or_create(id.clone());
        assert_eq!(entry.id, id);
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::Dm(dv) => assert!(dv.messages().is_empty()),
            _ => panic!("expected Dm variant"),
        }
    }

    #[test]
    fn pool_get_or_create_new_node_log_buffer() {
        let mut pool = BufferPool::new();
        let id = BufferId::NodeLog { node_id: "node1".into() };
        let entry = pool.get_or_create(id.clone());
        assert_eq!(entry.id, id);
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::NodeLog { text, pending } => {
                assert!(text.is_empty());
                assert!(!pending);
            }
            _ => panic!("expected NodeLog variant"),
        }
    }

    #[test]
    fn pool_get_existing_buffer() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        pool.get_or_create(id.clone());
        let entry = pool.get(&id);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().id, id);
    }

    #[test]
    fn pool_get_nonexistent_returns_none() {
        let pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        assert!(pool.get(&id).is_none());
    }

    #[test]
    fn pool_get_mut_existing() {
        let mut pool = BufferPool::new();
        let id = BufferId::Dm { node_id: "node1".into() };
        pool.get_or_create(id.clone());
        assert!(pool.get_mut(&id).is_some());
    }

    #[test]
    fn pool_get_mut_nonexistent_returns_none() {
        let mut pool = BufferPool::new();
        assert!(pool.get_mut(&BufferId::Dm { node_id: "x".into() }).is_none());
    }

    #[test]
    fn pool_get_or_create_is_idempotent() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        // first call creates, push a message
        if let BufferContent::Channel(ref mut cv) = pool.get_or_create(id.clone()).content {
            cv.push("first");
        }
        pool.get_or_create(id.clone()).bump_version();
        // second call should return same buffer, not overwrite
        let entry = pool.get_or_create(id);
        assert_eq!(entry.content_version, 1);
        match &entry.content {
            BufferContent::Channel(cv) => assert_eq!(cv.messages().len(), 1),
            _ => panic!("expected Channel variant"),
        }
    }

    // ========== 3. content_version ==========

    #[test]
    fn new_buffer_version_is_zero() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id);
        assert_eq!(entry.content_version, 0);
    }

    #[test]
    fn bump_version_increments() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id);
        entry.bump_version();
        assert_eq!(entry.content_version, 1);
        entry.bump_version();
        assert_eq!(entry.content_version, 2);
    }

    #[test]
    fn push_then_bump_version_is_precise() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id);
        // 设计要求：push 内容后调用 bump_version，版本从 0→1
        if let BufferContent::Channel(ref mut cv) = entry.content {
            cv.push("hello");
        }
        entry.bump_version();
        assert_eq!(entry.content_version, 1);
    }

    // ========== 4. DM buffer 生命周期 ==========

    #[test]
    fn dm_reenter_clears_messages_and_resets_version() {
        let mut pool = BufferPool::new();
        let id = BufferId::Dm { node_id: "node1".into() };

        // 首次进入，push 一些消息
        let entry = pool.get_or_create(id.clone());
        if let BufferContent::Dm(ref mut dv) = entry.content {
            dv.push("old msg 1");
            dv.push("old msg 2");
        }
        entry.bump_version();
        entry.bump_version();
        assert_eq!(entry.content_version, 2);

        // 模拟 enter_dm：按 BufferId::Dm 找到已有 buffer，清空
        let entry = pool.get_mut(&id).unwrap();
        entry.clear();
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::Dm(dv) => {
                assert!(dv.messages().is_empty(), "DM messages should be empty after clear");
            }
            _ => panic!("expected Dm variant"),
        }
    }

    #[test]
    fn exit_dm_clear_buffer_and_verify_empty() {
        let mut pool = BufferPool::new();
        let id = BufferId::Dm { node_id: "node1".into() };
        let entry = pool.get_or_create(id.clone());
        if let BufferContent::Dm(ref mut dv) = entry.content {
            dv.push("msg");
        }
        entry.bump_version();

        // exit_dm: 清空
        let entry = pool.get_mut(&id).unwrap();
        entry.clear();
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::Dm(dv) => {
                assert!(dv.messages().is_empty(), "DM messages should be empty after exit");
            }
            _ => panic!("expected Dm variant"),
        }
    }

    // ========== 5. ChannelBuffer 独立实例 ==========

    #[test]
    fn channel_buffers_are_independent() {
        let mut pool = BufferPool::new();
        let ch1 = BufferId::Channel { channel_id: "ch1".into() };
        let ch2 = BufferId::Channel { channel_id: "ch2".into() };

        pool.get_or_create(ch1.clone());
        pool.get_or_create(ch2.clone());

        // push to ch1 only
        let e1 = pool.get_mut(&ch1).unwrap();
        if let BufferContent::Channel(ref mut cv) = e1.content {
            cv.push("msg for ch1");
        }
        e1.bump_version();

        // ch2 should be unaffected
        let e2 = pool.get(&ch2).unwrap();
        assert_eq!(e2.content_version, 0);
        match &e2.content {
            BufferContent::Channel(cv) => assert!(cv.messages().is_empty()),
            _ => panic!("expected Channel variant"),
        }
    }

    // ========== 6. 非活跃频道收消息 ==========

    #[test]
    fn push_to_inactive_channel_via_pool() {
        let mut pool = BufferPool::new();
        let active = BufferId::Channel { channel_id: "active".into() };
        let inactive = BufferId::Channel { channel_id: "inactive".into() };

        pool.get_or_create(active.clone());
        pool.get_or_create(inactive.clone());

        // 直接通过 pool 找到非活跃频道 push + bump
        let entry = pool.get_mut(&inactive).unwrap();
        if let BufferContent::Channel(ref mut cv) = entry.content {
            cv.push("background msg");
        }
        entry.bump_version();

        assert_eq!(entry.content_version, 1);
        match &entry.content {
            BufferContent::Channel(cv) => {
                assert_eq!(cv.messages().len(), 1);
                assert_eq!(cv.messages()[0], "background msg");
            }
            _ => panic!("expected Channel variant"),
        }
    }

    // ========== 7. NodeLog ==========

    #[test]
    fn node_log_buffer_append_text_with_version() {
        let mut pool = BufferPool::new();
        let id = BufferId::NodeLog { node_id: "node1".into() };
        let entry = pool.get_or_create(id);

        // 两次追加
        if let BufferContent::NodeLog { ref mut text, ref mut pending } = entry.content {
            text.push_str("log line 1\n");
            *pending = true;
        }
        entry.bump_version();

        if let BufferContent::NodeLog { ref mut text, .. } = entry.content {
            text.push_str("log line 2\n");
        }
        entry.bump_version();

        assert_eq!(entry.content_version, 2);
        match &entry.content {
            BufferContent::NodeLog { text, pending } => {
                assert!(text.contains("log line 1"));
                assert!(text.contains("log line 2"));
                assert!(*pending);
            }
            _ => panic!("expected NodeLog variant"),
        }
    }

    #[test]
    fn node_log_pending_starts_false() {
        let mut pool = BufferPool::new();
        let id = BufferId::NodeLog { node_id: "node1".into() };
        let entry = pool.get_or_create(id);
        match &entry.content {
            BufferContent::NodeLog { pending, .. } => assert!(!pending),
            _ => panic!("expected NodeLog variant"),
        }
    }

    #[test]
    fn node_log_clear_resets_text_and_pending() {
        let mut pool = BufferPool::new();
        let id = BufferId::NodeLog { node_id: "node1".into() };
        let entry = pool.get_or_create(id.clone());
        if let BufferContent::NodeLog { ref mut text, ref mut pending } = entry.content {
            text.push_str("some log");
            *pending = true;
        }
        entry.bump_version();

        let entry = pool.get_mut(&id).unwrap();
        entry.clear();
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::NodeLog { text, pending } => {
                assert!(text.is_empty(), "NodeLog text should be empty after clear");
                assert!(!pending, "NodeLog pending should be false after clear");
            }
            _ => panic!("expected NodeLog variant"),
        }
    }

    // ========== 8. Channel clear ==========

    #[test]
    fn channel_clear_resets_messages_and_version() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id.clone());
        if let BufferContent::Channel(ref mut cv) = entry.content {
            cv.push("msg1");
        }
        entry.bump_version();

        let entry = pool.get_mut(&id).unwrap();
        entry.clear();
        assert_eq!(entry.content_version, 0);
        match &entry.content {
            BufferContent::Channel(cv) => {
                assert!(cv.messages().is_empty(), "Channel messages should be empty after clear");
            }
            _ => panic!("expected Channel variant"),
        }
    }

    // ========== 9. Window ==========

    #[test]
    fn window_new_initializes_last_seen_version_from_buffer() {
        // 模拟已有历史的 buffer（version=5）
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id.clone());
        for _ in 0..5 {
            entry.bump_version();
        }
        let current_version = entry.content_version;
        assert_eq!(current_version, 5);

        // 新开 window，last_seen_version 应等于 buffer 当前版本
        let window = Window::new(id, current_version);
        assert_eq!(window.last_seen_version, 5, "should not report new messages for existing history");
        assert!(window.auto_scroll, "new window should default to auto_scroll=true");
        assert!(!window.has_new_messages, "new window should not have new message indicator");
        assert_eq!(window.scroll_offset, 0);
    }

    #[test]
    fn window_new_with_zero_version() {
        let id = BufferId::Dm { node_id: "node1".into() };
        let window = Window::new(id.clone(), 0);
        assert_eq!(window.buffer_id, id);
        assert_eq!(window.last_seen_version, 0);
        assert!(window.auto_scroll);
        assert!(!window.has_new_messages);
    }

    // ========== 10. NodeLog window 关闭 → unsubscribe + pending=false ==========

    #[test]
    fn close_last_node_log_window_clears_pending() {
        let mut pool = BufferPool::new();
        let id = BufferId::NodeLog { node_id: "node1".into() };
        let entry = pool.get_or_create(id.clone());

        // 模拟有流式内容
        if let BufferContent::NodeLog { ref mut text, ref mut pending } = entry.content {
            text.push_str("streaming log...");
            *pending = true;
        }
        entry.bump_version();

        // 模拟关闭最后一个 NodeLog window：
        // 检查 pool 中 buffer 存在，设置 pending=false
        let entry = pool.get_mut(&id).unwrap();
        match &mut entry.content {
            BufferContent::NodeLog { pending, .. } => {
                assert!(*pending, "pending should be true before close");
                *pending = false;
            }
            _ => panic!("expected NodeLog variant"),
        }

        // 验证关闭后状态
        let entry = pool.get(&id).unwrap();
        match &entry.content {
            BufferContent::NodeLog { text, pending } => {
                // buffer 保留在 pool 中（只读），text 不清空
                assert!(!text.is_empty(), "buffer content should be preserved");
                assert!(!pending, "pending should be false after close");
            }
            _ => panic!("expected NodeLog variant"),
        }
    }
}
