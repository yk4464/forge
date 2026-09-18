# forge

⚡ 一个用 Rust 写的、流式优先的终端编程 Agent（v0.1.0）。

[English](README_EN.md) | 中文

forge 在终端里运行：你用自然语言下指令，它通过一个 `shell` 工具执行命令（读写文件、搜索、构建、跑测试……），流式输出模型正文与推理内容，并展示工具结果，直到任务完成。上下文接近模型窗口时自动压缩（Codex 风格）；当前已知缺口及修复顺序见 [ROADMAP.md](ROADMAP.md)。

## 特性

- **流式事件接口** — 正文、推理内容和工具事件通过统一的 `AgentEvent` 接口驱动 Ratatui TUI；shell 输出实时转发到界面
- **三个协议面，一套 trait** — `/v1/chat/completions`（OpenAI 兼容，覆盖 DeepSeek/智谱/通义）、`/v1/responses`、`/v1/messages`（Anthropic 格式），全部 SSE 流式，`reasoning_content` / `thinking` 独立事件
- **Codex 风格上下文压缩** — token 记账（本地估算 + API usage 锚定），pre-turn 与每次工具输出后双检查点，超阈值自动压缩历史；`/compact` 可手动触发
- **shell 工具** — Git Bash 执行命令、超时控制（默认 120s / 上限 600s）、实时输出转发与有界捕获（每流 1 MiB，head+tail + 省略标记）；超时保留已捕获输出，整个进程树经 Job Object 在超时/取消/命令结束时回收
- **SQLite 会话持久化** — WAL 模式；`/resume` 恢复会话，跨进程也有记忆
- **可扩展内核** — 已有 Provider、Tool、Registry、会话存储与权限接口；权限层当前默认放行，Skills / Hooks 的完整契约与实现列入后续阶段

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
| `Esc` | 取消当前任务（运行中生效） |
| `/help` | 帮助 |

## 配置

见 [config.example.toml](config.example.toml)，全部字段带注释：协议面（`openai` / `responses` / `anthropic`）、base_url、API key、模型、上下文窗口大小、自动压缩阈值、shell 路径与超时。

## Roadmap

详见 [ROADMAP.md](ROADMAP.md)：包含完整功能清单、依赖、实现顺序和阶段验收，不承诺日历工期。v0.2.0 已有核心循环、TUI、三协议面、SQLite、上下文压缩、任务取消、单轮预算与崩溃恢复；S0/S1 均已完成（2026-09-19），S2 推进中。

- [x] **S0 修复基线**：shell 流式输出与内存上限、异常结束、压缩消息完整性（2026-09-19 完成）
- [x] **S1 可控运行与数据可靠性**：取消、原始记录保真、故障恢复、任务预算、运行接口（2026-09-19 完成）
- [ ] **S2 安全编程闭环**：权限与工作区边界、文件编辑/搜索、项目规则、计划与回退
- [ ] **S3 日常可用**：Provider 恢复、会话与 TUI、上下文与用量、脚本接口、基础发行包
- [ ] **S4 扩展生态**：Skills、MCP、Hooks、后台任务与联网工具
- [ ] **S5 多 agent 协作**：worktree、子代理、跨终端消息
- [ ] **S6 多端交付**：Web UI（Axum + React）、Tauri 2 桌面端
- [ ] **S7 按需增强**：LSP、PTY、OAuth/订阅额度、额外平台与自动更新

S5 与 S6 可按需求调换；运行接口在 S1 预留，测试与评测贯穿各阶段。

## License

[MIT](LICENSE)
