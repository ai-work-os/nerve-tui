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

    /// 检查 buffer 的 content_version，决定是否 snap_to_bottom 或标记 has_new_messages
    /// 返回 true 表示需要 snap_to_bottom
    pub fn check_content_version(&mut self, buffer_version: u64) -> bool {
        if buffer_version > self.last_seen_version {
            self.last_seen_version = buffer_version;
            if self.auto_scroll {
                true
            } else {
                self.has_new_messages = true;
                false
            }
        } else {
            false
        }
    }
}

/// 焦点位置
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowFocus {
    Primary,
    Panel(usize),
}

/// 窗口布局：primary + panels
pub struct WindowLayout {
    /// 主窗口（左侧，始终存在）
    pub primary: Window,
    /// 右侧面板（0..N）
    pub panels: Vec<Window>,
    /// 焦点位置
    pub focus: WindowFocus,
    /// 面板左边界 x 坐标缓存
    pub panel_x_boundaries: Vec<u16>,
}

impl WindowLayout {
    /// 创建新布局，primary 窗口必须提供
    pub fn new(primary: Window) -> Self {
        Self {
            primary,
            panels: Vec::new(),
            focus: WindowFocus::Primary,
            panel_x_boundaries: Vec::new(),
        }
    }

    /// 添加 panel 窗口
    pub fn add_panel(&mut self, window: Window) {
        self.panels.push(window);
        self.panel_x_boundaries.push(0);
    }

    /// 关闭指定 panel，focus 自动 clamp
    pub fn remove_panel(&mut self, index: usize) {
        if index >= self.panels.len() {
            return;
        }
        self.panels.remove(index);
        self.panel_x_boundaries.pop();
        if self.panels.is_empty() {
            self.focus = WindowFocus::Primary;
        } else if let WindowFocus::Panel(i) = self.focus {
            if i >= self.panels.len() {
                self.focus = WindowFocus::Panel(self.panels.len() - 1);
            }
        }
    }

    /// panel 数量
    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    /// 焦点循环：Primary → Panel(0) → Panel(1) → ... → Primary
    pub fn cycle_focus_forward(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        self.focus = match self.focus {
            WindowFocus::Primary => WindowFocus::Panel(0),
            WindowFocus::Panel(i) => {
                if i + 1 < self.panels.len() {
                    WindowFocus::Panel(i + 1)
                } else {
                    WindowFocus::Primary
                }
            }
        };
    }

    /// 检查 panels 中是否还有引用指定 buffer_id 的窗口
    pub fn has_panel_for_buffer(&self, buffer_id: &BufferId) -> bool {
        self.panels.iter().any(|w| &w.buffer_id == buffer_id)
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

    // ==========================================================
    // Phase 2: Window + WindowLayout 测试
    // NOTE: unsubscribe 调用测试在 App 集成测试中覆盖
    // ==========================================================

    // ========== 11. WindowLayout 基础 ==========

    #[test]
    fn window_layout_new_has_primary_and_empty_panels() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let layout = WindowLayout::new(primary);
        assert_eq!(layout.panel_count(), 0);
        assert_eq!(layout.focus, WindowFocus::Primary);
        assert_eq!(layout.primary.buffer_id, BufferId::Channel { channel_id: "ch1".into() });
    }

    // ========== 12. 添加 panel window ==========

    #[test]
    fn layout_add_panel_increments_count() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);

        let panel = Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0);
        layout.add_panel(panel);
        assert_eq!(layout.panel_count(), 1);

        let panel2 = Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0);
        layout.add_panel(panel2);
        assert_eq!(layout.panel_count(), 2);
    }

    // ========== 13. WindowFocus 循环 ==========

    #[test]
    fn focus_cycle_with_no_panels() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        assert_eq!(layout.focus, WindowFocus::Primary);
        // 无 panel 时 cycle 应停留在 Primary
        layout.cycle_focus_forward();
        assert_eq!(layout.focus, WindowFocus::Primary);
    }

    #[test]
    fn focus_cycle_through_panels_and_back() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);

        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));

        assert_eq!(layout.focus, WindowFocus::Primary);
        layout.cycle_focus_forward();
        assert_eq!(layout.focus, WindowFocus::Panel(0));
        layout.cycle_focus_forward();
        assert_eq!(layout.focus, WindowFocus::Panel(1));
        layout.cycle_focus_forward();
        assert_eq!(layout.focus, WindowFocus::Primary);
    }

    // ========== 14. 关闭 panel → focus clamp ==========

    #[test]
    fn remove_panel_clamps_focus_to_primary() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));

        // focus 在 Panel(0)，关闭该 panel 后 focus 应回到 Primary
        layout.cycle_focus_forward();
        assert_eq!(layout.focus, WindowFocus::Panel(0));
        layout.remove_panel(0);
        assert_eq!(layout.panel_count(), 0);
        assert_eq!(layout.focus, WindowFocus::Primary);
    }

    #[test]
    fn remove_panel_clamps_focus_index() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));

        // focus 在 Panel(1)，关闭 Panel(1) 后 focus 应 clamp 到 Panel(0)
        layout.cycle_focus_forward(); // Panel(0)
        layout.cycle_focus_forward(); // Panel(1)
        assert_eq!(layout.focus, WindowFocus::Panel(1));
        layout.remove_panel(1);
        assert_eq!(layout.panel_count(), 1);
        assert_eq!(layout.focus, WindowFocus::Panel(0));
    }

    // ========== 15. content_version 驱动 auto_scroll ==========

    #[test]
    fn check_version_with_auto_scroll_returns_snap() {
        let mut window = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        assert!(window.auto_scroll);

        // buffer version 递增到 3
        let should_snap = window.check_content_version(3);
        assert!(should_snap, "auto_scroll=true + new version should snap");
        assert_eq!(window.last_seen_version, 3);
        assert!(!window.has_new_messages);
    }

    #[test]
    fn check_version_same_version_no_snap() {
        let mut window = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 5);
        let should_snap = window.check_content_version(5);
        assert!(!should_snap, "same version should not snap");
        assert!(!window.has_new_messages);
    }

    // ========== 16. content_version 驱动 has_new_messages ==========

    #[test]
    fn check_version_without_auto_scroll_sets_has_new_messages() {
        let mut window = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        window.auto_scroll = false;

        let should_snap = window.check_content_version(2);
        assert!(!should_snap, "auto_scroll=false should not snap");
        assert!(window.has_new_messages, "should mark has_new_messages");
        assert_eq!(window.last_seen_version, 2);
    }

    // ========== 17. 已有历史 buffer 新开 window 不误报 ==========

    #[test]
    fn window_on_existing_buffer_no_false_new_messages() {
        let mut pool = BufferPool::new();
        let id = BufferId::Channel { channel_id: "ch1".into() };
        let entry = pool.get_or_create(id.clone());
        // 模拟已有 10 条消息
        for _ in 0..10 {
            if let BufferContent::Channel(ref mut cv) = entry.content {
                cv.push("msg");
            }
            entry.bump_version();
        }
        assert_eq!(entry.content_version, 10);

        // 新开 window，用 buffer 当前 version
        let mut window = Window::new(id, entry.content_version);
        // check 同版本不应触发任何指示
        let should_snap = window.check_content_version(10);
        assert!(!should_snap);
        assert!(!window.has_new_messages);
    }

    // ========== 18. NodeLog window 全关闭检测 ==========

    #[test]
    fn has_panel_for_buffer_after_all_removed() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        let log_id = BufferId::NodeLog { node_id: "n1".into() };

        layout.add_panel(Window::new(log_id.clone(), 0));
        layout.add_panel(Window::new(log_id.clone(), 0));
        assert!(layout.has_panel_for_buffer(&log_id));

        // 关闭第一个，还有引用
        layout.remove_panel(0);
        assert!(layout.has_panel_for_buffer(&log_id));

        // 关闭最后一个，无引用
        layout.remove_panel(0);
        assert!(!layout.has_panel_for_buffer(&log_id), "no panels should reference this buffer");
    }

    // ========== 19. primary buffer_id 跟随切换 ==========

    #[test]
    fn primary_buffer_id_follows_channel_switch() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);

        // 模拟切频道：更新 primary.buffer_id
        let new_id = BufferId::Channel { channel_id: "ch2".into() };
        layout.primary = Window::new(new_id.clone(), 0);
        assert_eq!(layout.primary.buffer_id, new_id);
    }

    #[test]
    fn primary_buffer_id_follows_dm_enter() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);

        // 模拟进 DM：更新 primary.buffer_id
        let dm_id = BufferId::Dm { node_id: "node1".into() };
        layout.primary = Window::new(dm_id.clone(), 0);
        assert_eq!(layout.primary.buffer_id, dm_id);
    }

    #[test]
    fn primary_switch_preserves_panels() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));

        // 切频道后 panels 不受影响
        layout.primary = Window::new(BufferId::Channel { channel_id: "ch2".into() }, 0);
        assert_eq!(layout.panel_count(), 1);
    }

    // ========== 20. panel_x_boundaries 缓存 ==========

    #[test]
    fn panel_x_boundaries_length_matches_panels() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        assert_eq!(layout.panel_x_boundaries.len(), 0);

        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));
        assert_eq!(layout.panel_x_boundaries.len(), layout.panel_count());

        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n2".into() }, 0));
        assert_eq!(layout.panel_x_boundaries.len(), layout.panel_count());

        layout.remove_panel(0);
        assert_eq!(layout.panel_x_boundaries.len(), layout.panel_count());
    }

    // ========== 21. 越界 remove 边界 ==========

    #[test]
    fn remove_panel_out_of_bounds_is_noop() {
        let primary = Window::new(BufferId::Channel { channel_id: "ch1".into() }, 0);
        let mut layout = WindowLayout::new(primary);
        layout.add_panel(Window::new(BufferId::NodeLog { node_id: "n1".into() }, 0));

        // remove 超出范围的 index 不应 panic，panel 数不变
        layout.remove_panel(5);
        assert_eq!(layout.panel_count(), 1);

        // 空 panels 时 remove 也不应 panic
        layout.remove_panel(0);
        layout.remove_panel(0);
        assert_eq!(layout.panel_count(), 0);
    }
}
