# DM 模式增强方案

基于 nvim nerve 插件（TypeScript）和 nerve server 源码分析，为 nerve-tui 补齐 DM 模式的关键能力。

## 当前 nerve-tui 缺什么

| 能力 | 状态 | 说明 |
|------|------|------|
| DM 进入/退出/收发消息 | 已完成 | subscribe/unsubscribe、streaming、replay |
| streaming 落盘 | 已完成 | idle flush + user_message flush + agent_message_end |
| node.cancel API | 已有 | ws_client.rs `node_cancel()`，app.rs `/cancel` 命令 |
| **busy 状态追踪** | 缺失 | DmState 没有 busy flag，无法区分等待回复 vs 空闲 |
| **回复中禁止发送** | 缺失 | agent busy 时用户仍可发消息（server 会自动 cancel 旧 prompt） |
| **Esc 中断回复** | 缺失 | DM 模式 Esc 只退出 DM，无法 cancel 进行中的回复 |
| **cancel UI 反馈** | 缺失 | cancel 后无 streaming 清理，无系统提示 |
| **频道内 agent 过滤** | 已完成 | Ctrl+N/P 切换 tab，filter by name + @mention |

## 从 nvim 插件学到的模式

### 1. Auto-cancel on busy

nerve server 的 `bus.dispatchDirect()` 在发送 prompt 前检查 `node.status === "busy"`，自动先 cancel 再发送新 prompt。这意味着：
- 客户端无需阻止用户发消息（server 兜底）
- 但 UX 上应告知用户"上一条回复被中断"

来源：`nerve/src/bus.ts:237-242`

### 2. Status 驱动 UI

server 通过 `node.statusChanged` 广播状态变化（busy/idle/error）。nvim 插件用这个信号：
- 更新 agent 状态图标
- idle 时允许新 prompt
- busy 时显示"正在回复..."

来源：`nerve/src/node-pool.ts:180-196`，`nerve/src/server.ts:618-649`

### 3. Cancel = notification，不是 request

ACP 协议层 `session/cancel` 是 notification（不等待 response）。nerve server 收到后：
1. 调 `acpClient.cancel()`
2. 重置 `node.status = "idle"`
3. 广播 `node.statusChanged`

客户端应在 `statusChanged(idle)` 回来后才认为 cancel 完成。

来源：`nerve/src/acp-client.ts:177-191`，`nerve/src/node-pool.ts:200-212`

### 4. 频道过滤是客户端行为

nerve server 的 `channel.history` 返回所有消息，不做 per-agent 过滤。过滤完全在客户端实现。nerve-tui 当前的实现（filter by from + @mention）已经是正确方式。

## 具体改动方案

### 改动 1：DmState 加 busy 标志

**文件**: `nerve-tui-protocol/src/types.rs`

```rust
pub struct DmState {
    pub node_id: String,
    pub node_name: String,
    pub messages: Vec<DmMessage>,
    pub streaming: Option<String>,
    pub busy: bool,  // 新增
}
```

### 改动 2：busy 状态跟踪

**文件**: `nerve-tui/src/app.rs`

- 发送 prompt 时：`active_dm.busy = true`
- 收到 `NodeStatusChanged { status: "idle", .. }` 时：`active_dm.busy = false`
- 收到 `NodeStatusChanged { status: "busy", .. }` 时：`active_dm.busy = true`

```rust
// handle_nerve_event → NodeStatusChanged
if let Some(ref mut dm) = self.active_dm {
    if dm.node_id == node_id {
        dm.busy = status == "busy";
    }
}
```

### 改动 3：回复中禁止发送（软阻止 + 提示）

**文件**: `nerve-tui/src/app.rs` → `handle_input()`

DM 模式下，如果 `active_dm.busy == true`，有两种策略：

**方案 A（推荐）：提示但允许发送**
```rust
if let Some(ref dm) = self.active_dm {
    if dm.busy {
        self.messages.push_system("agent 正在回复，新消息将中断当前回复");
    }
    // 继续发送（server 会 auto-cancel）
}
```

**方案 B：阻止发送**
```rust
if let Some(ref dm) = self.active_dm {
    if dm.busy {
        self.messages.push_system("agent 正在回复，请等待或按 Esc 中断");
        return;
    }
}
```

推荐方案 A：和 server 行为一致（auto-cancel），用户体验更好。

### 改动 4：Esc 键 cancel 逻辑

**文件**: `nerve-tui/src/app.rs` → `handle_key()` 的 Esc 分支

当前 Esc 直接退出 DM。改为：
- DM + busy → cancel agent（不退出 DM）
- DM + idle → 退出 DM
- 非 DM → 关闭补全 popup

```rust
KeyCode::Esc => {
    if let Some(ref dm) = self.active_dm {
        if dm.busy {
            // Cancel agent reply
            let node_id = dm.node_id.clone();
            if let Err(e) = self.client.node_cancel(&node_id).await {
                warn!("node.cancel failed: {}", e);
            }
            self.messages.push_system("已中断 agent 回复");
            self.messages.streaming.retain(|(n, _)| n != &dm.node_name);
            // busy flag 会在 NodeStatusChanged(idle) 回来时清除
        } else {
            self.exit_dm().await;
        }
    } else {
        self.input.dismiss_popup();
    }
}
```

### 改动 5：输入框 busy 状态指示

**文件**: `nerve-tui/src/components/input.rs`

输入框标题根据 DM busy 状态变化：
- idle: `输入消息...`
- busy: `agent 正在回复... (Esc 中断)`

需要在 App 渲染时传递 busy 状态给 InputBox。

```rust
// app.rs render()
let dm_busy = self.active_dm.as_ref().map_or(false, |dm| dm.busy);
self.input.render_with_state(layout.input, frame.buffer_mut(), dm_busy);
```

### 改动 6：cancel 后清理 streaming

**文件**: `nerve-tui/src/app.rs`

在 `NodeStatusChanged` 处理中，如果状态从 busy→idle 且有 streaming buffer，先 flush 再清除 busy。当前的 `flush_streaming_as_dm()` 已经做了这个。

需要额外处理的是：cancel 后可能收不到完整的 streaming content（agent 被打断）。需要判断是否要丢弃不完整的 streaming：

```rust
// NodeStatusChanged(idle) 且 is cancel 场景
// 保守方案：仍然 flush（展示已收到的部分回复）
// 激进方案：丢弃（因为不完整）
// 推荐保守方案
```

## 改动文件清单

| 文件 | 改动 |
|------|------|
| `nerve-tui-protocol/src/types.rs` | DmState 加 `busy: bool` |
| `nerve-tui/src/app.rs` | busy 追踪、Esc cancel、send 提示、渲染传参 |
| `nerve-tui/src/components/input.rs` | busy 状态标题显示 |
| `nerve-tui/src/components/messages.rs` | 无改动（现有 streaming 渲染已满足） |
| `nerve-tui/src/components/status_bar.rs` | 无改动（已显示 agent status） |

## 优先级

1. **P0**: busy 状态追踪 + Esc cancel（核心 UX）
2. **P1**: send 提示（安全网）
3. **P2**: input 框 busy 指示（可选美化）
