# forge

⚡ 一个用 Rust 写的、流式优先的终端编程 Agent（Milestone 1）。

[English](README_EN.md) | 中文

forge 在终端里运行：你用自然语言下指令，它通过一个 `shell` 工具执行命令（读写文件、搜索、构建、跑测试……），流式输出思考过程和结果，直到任务完成。上下文接近模型窗口时自动压缩（Codex 风格），长会话不崩。

## 特性

- **流式一等公民** — 正文、思维链、工具输出全部通过统一的 `AgentEvent` 事件流实时推送；Ratatui TUI 边收边渲染
- **三个协议面，一套 trait** — `/v1/chat/completions`（OpenAI 兼容，覆盖 DeepSeek/智谱/通义）、`/v1/responses`、`/v1/messages`（Anthropic 格式），全部 SSE 流式，`reasoning_content` / `thinking` 独立事件
- **Codex 风格上下文压缩** — token 记账（本地估算 + API usage 锚定），pre-turn 与每次工具输出后双检查点，超阈值自动压缩历史；`/compact` 可手动触发
- **shell 工具打天下** — Git Bash 执行命令，实时输出流、超时控制（默认 120s / 上限 600s）、1 MiB 捕获上限 + 中间截断
- **SQLite 会话持久化** — WAL 模式；`/resume` 恢复会话，跨进程也有记忆
- **可扩展内核** — `ModelProvider` / `Tool` / `Skill` / `Hook` / `Permission` 全部是 trait + Registry；权限层默认放行，架构留位

## 架构

```
forge-tui ──► forge-core ──► forge-provider   (chat/completions · responses · messages)
                 │    ╰────► forge-tools      (shell)
                 ╰──────────► forge-storage   (SQLite + SQLx)
```

`forge-core` 定义全部 trait（`ModelProvider` / `Tool` / `AgentEvent` / 压缩器 / 会话存储接口），零 UI、零 HTTP 依赖；协议实现、工具实现、存储实现都在 core 之外，靠 Registry 组装。新增一个协议面 = 实现一个 trait + 注册，不改循环。

## 平台要求

- **Windows** + [Git for Windows](https://gitforwindows.org/)（shell 工具调用 `bash.exe`，自动探测路径，也可在配置中指定）
- Rust stable（edition 2021）

## 快速开始

```bash
git clone https://github.com/yk4464/forge.git
cd forge
cp config.example.toml config.toml   # 填入你的 base_url / key / 模型名
cargo run -p forge-tui               # 进入 TUI（或 cargo build --release 后运行 target/release/forge）
```

API key 二选一：写在 `config.toml` 的 `provider.api_key`（文件已被 gitignore），或设置 `config.toml` 里 `provider.api_key_env` 指定的环境变量。

无头模式（脚本/验收用，连续调用共享同一个会话）：

```bash
forge check "查看当前目录结构并总结"
```

## TUI 命令

| 命令 | 作用 |
|------|------|
| `/new` | 新建会话 |
| `/resume` | 恢复最近的会话 |
| `/compact` | 手动触发上下文压缩 |
| `/exit` | 退出 |
| `/help` | 帮助 |

## 配置

见 [config.example.toml](config.example.toml)，全部字段带注释：协议面（`openai` / `responses` / `anthropic`）、base_url、API key、模型、上下文窗口大小、自动压缩阈值、shell 路径与超时。

## Roadmap

- [x] M1：核心循环 + TUI + 三协议面 + SQLite + 上下文压缩
- [ ] M2：Web UI（Axum + React）
- [ ] M2：Tauri 2 桌面端（复用 Web UI）
- [ ] MCP（rmcp）接入
- [ ] Skills / Hooks 具体实现（trait 占位已在）
- [ ] 权限策略细化（逐工具审批）

## License

[MIT](LICENSE)
