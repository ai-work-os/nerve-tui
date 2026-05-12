# Nerve TUI

TUI 客户端 — 终端里的 nerve 聊天界面，主力客户端。

## 架构

- 纯事件驱动，消息只追加不重建
- 无本地持久化，server buffer 是 source of truth
- 重连只重新 subscribe，不清状态
- Android 以本项目为参考标准

## 技术栈

- Rust + crossterm + ratatui
- 异步运行时：tokio

## 测试

```bash
cd nerve-tui
cargo test
```

## 构建 & 安装

```bash
nerve-server build tui
nerve-server install tui
```

## 关键模块

| 文件 | 职责 |
|------|------|
| `src/dm.rs` | DM 聊天核心（消息管理、streaming） |
| `src/connection.rs` | WebSocket 连接管理、重连 |
| `src/types.rs` | 数据类型定义 |
| `src/app.rs` | 应用主循环、事件分发 |
| `src/ui/` | 渲染层 |

## 日志

关键路径有 tracing 日志，排查时用 `RUST_LOG=debug` 启动。

## 参考

- 遇到终端渲染问题先看 ratatui 官方做法
- Android 端对标本项目实现
