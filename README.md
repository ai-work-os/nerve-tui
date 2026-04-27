# nerve-tui

AI Work OS 的终端主力客户端。

`nerve-tui` 是人在终端里驾驭 nerve 的 cockpit：连接 nerve server，看所有 agent 和频道状态，和 AI 1v1 对话，在频道里分派任务、观察多 agent 协作过程。

## 它在系统里的位置

`nerve` 是服务端，`nerve-tui` 是当前最常用的人机界面。

它不保存业务真相，也不自己编排 agent。它做三件事：

- 把 nerve server 里的节点、频道、消息实时呈现出来。
- 给人一个高效输入面板，能 DM、发频道消息、执行命令。
- 让多 agent 协作过程可见：谁在干活、谁卡住、谁回复了什么。

## 当前做到什么程度

已是日常可用客户端。

已完成能力：

- 连接本地或远端 nerve server。
- 查看 agent、程序节点、客户端节点。
- DM 1v1 聊天，支持流式输出、多轮对话、历史 replay。
- 频道协作，支持 `@agent` 分派任务。
- 多频道切换、未读提示、频道历史恢复。
- 中断 agent 回复。
- Markdown 渲染、代码块高亮、tool_call / tool_result 结构化展示。
- 多行输入、命令补全、剪贴板、滚动、分屏观察。
- 日志写入 `~/.nerve/tui.log`，方便排查。

它支撑了已经验证过的协作模式：

- tester / coder / reviewer 自主管线。
- sub-main 调度多个 worker 并更新进展文件。
- 人合盖离开后，AI 继续在频道里协作，回来后直接看结果。

## 当前阶段

TUI 已完成 M2.5 和 M5 的主体目标。

当前不是继续堆 UI 功能，而是服务于 M6.5/M7：

- 更好地观察多个 sub-main 和 worker 的状态。
- 支撑 AI 自主协作过程中的进展查看。
- 让人在关键节点快速确认方案或打回。
- 后续配合 harness，把“看起来完成”变成“有证据完成”。

## 未来方向

短期：

- 分屏能力继续收敛到“看任意节点输出”。
- 侧边栏残留节点、手动刷新等小问题继续修。
- 增强集成测试，避免 TUI “测试绿但真实路径没接上”。

长期：

- TUI 仍是 Mac 上的主控台。
- 手机端负责移动场景。
- server 端 duty-monitor 和值班 AI 负责无人值守。

## 仓库关系

| 仓库 | 作用 |
|---|---|
| `nerve` | 服务端，TUI 通过 WebSocket JSON-RPC 连接它 |
| `nerve-tui` | 终端客户端 |
| `nerve-app` | Android 新客户端 |
| `nerve-android` | Android 旧客户端 |

## 常用命令

```bash
# 构建
cargo build

# 测试
cargo test

# 集成测试
cargo test -p nerve-tui-core --features integration -- --test-threads=1

# 运行
cargo run -p nerve-tui-bin -- --server ws://localhost:4800
```

统一脚本：

```bash
nerve-server build tui
nerve-server install tui
```

## 开发约束

- 先在 worktree 的 `dev` 分支开发，验证后合回主仓库。
- 涉及行为变化必须先写测试。
- TUI 任务不能只看单元测试，真实渲染路径也要验证。
- 客户端不替 server 做业务真相；server-driven 是边界。
