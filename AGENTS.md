# AGENTS.md — forge 工作区指南

## 项目定位

forge：Rust 编写的流式优先终端编程 Agent（v0.0.1 原型）。单一 shell 工具，
三个 LLM 协议面，Codex 风格上下文压缩，SQLite 会话持久化，Ratatui TUI。

## 结构（Cargo workspace，5 crates）

- `crates/forge-core` — 全部核心 trait（`ModelProvider`/`Tool`/`Skill`/`Hook`/`Permission`）、
  agent loop（`agent_loop.rs`）、压缩器（`compact.rs`）、上下文记账（`context.rs`）、
  `AgentEvent` 事件总线。零 UI、零 HTTP 依赖。
- `crates/forge-provider` — 三个 SSE 协议面：`openai.rs`（/v1/chat/completions）、
  `responses.rs`（/v1/responses）、`anthropic.rs`（/v1/messages）。
- `crates/forge-tools` — shell 工具（Git Bash）。
- `crates/forge-storage` — SQLx + SQLite（WAL），sessions/messages 表。
- `crates/forge-tui` — Ratatui 前端，二进制名 `forge`（`forge check "..."` 为无头模式）。

## 常用命令

```bash
cargo build --workspace          # 必须零警告
cargo test --workspace           # 当前 67 个测试
cargo run -p forge-tui           # 跑 TUI
target/debug/forge check "..."   # 无头验收（复用 check 会话）
```

## 架构铁律

- 依赖方向：`forge-tui → forge-core → {forge-provider, forge-tools, forge-storage}`，
  实现链为 provider/tools/storage 实现 core 的 trait。**禁止 core 依赖任何实现方或 UI/HTTP 库**。
- 新增协议面 = 在 forge-provider 实现 `ModelProvider` trait + 在其 `from_config`
  工厂注册，不改 agent loop。
- 事件流：core 经 `tokio::sync::mpsc::UnboundedSender<AgentEvent>` 推送；TUI 用
  `try_recv` 排水。新事件类型加在 `forge-core/src/event.rs`。

## 平台与已知坑

- 目标平台 **Windows + Git Bash**（shell 工具 spawn `bash.exe -c`）；CI 只跑
  `windows-latest`。
- **Provider SSE 测试必须** `#[tokio::test(flavor = "multi_thread")]` +
  `spawn_blocking` 里 accept TcpListener——单线程 runtime 在 Windows 上会挂死
  （现有 `crates/forge-provider/tests/*` 是模板）。
- 压缩常量（`context.rs`/`compact.rs`，清洁室复刻 Codex）：有效窗口 =
  min(原始窗口 × 0.95, 原始窗口 − max_output_tokens)，自动压缩阈值 = 有效窗口 × 0.90；
  裁剪必须保持工具调用/结果成对。
- 多轮记忆靠 `Agent` 跨轮持久 + 启动时回放 SQLite 历史（`build_agent`）；不要
  在每轮重建 Agent。
- shell 实时转发靠 `execute` 任务上的 select 循环并发驱动（`emit` 非 `'static`
  不能跨 spawn，管道块经通道回送本任务）；超时后排水仅 250ms（结果须贴近
  截止时间落地），正常退出排水上限 1.5s（孙进程握住管道写端时兜底）——改
  `shell.rs` 时保持这两个界限。

## 必读文档

- `ROADMAP.md` — **S0–S7 分阶段计划**。S0 基线问题已于 2026-09-19 全部修复
  （S0 清单逐条标注了 commit）；改 `shell.rs`/`compact.rs` 前先读对应节与现有
  回归测试，别把已修项当新发现。
- `README.md` / `README_EN.md` — 功能边界按此表述，不要夸大未实现特性。

## 约定

- 秘密零入库：`config.toml`（含真实 key）已被 gitignore，示例只进 `config.example.toml`。
- 提交信息用英文 conventional 风格（`docs:` `chore:` `feat:` `fix:`）。
- 文档：README 中文为主 + README_EN 英文同步；代码注释英文。
- 版本统一在根 `Cargo.toml` `[workspace.package]`，各 crate `version.workspace = true`。
