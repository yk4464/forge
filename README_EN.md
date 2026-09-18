# forge

⚡ A streaming-first terminal coding agent written in Rust (v0.0.1 prototype).

English | [中文](README.md)

forge runs in your terminal: you give instructions in natural language, it executes commands through a single `shell` tool (read/write files, search, build, run tests…), streams model text and reasoning, displays tool results, and keeps going until the task is done. When the context approaches the model's window, it auto-compacts (Codex-style). Known gaps and the planned fixes are tracked in [ROADMAP.md](ROADMAP.md).

## Features

- **Streaming event interface** — assistant text, reasoning, and tool events drive the Ratatui TUI through `AgentEvent`; shell output is forwarded to the UI live
- **Three wire protocols, one trait** — `/v1/chat/completions` (OpenAI-compatible: DeepSeek, Zhipu, Qwen, …), `/v1/responses`, and `/v1/messages` (Anthropic format); all SSE-streamed, with `reasoning_content` / `thinking` surfaced as separate events
- **Codex-style context compaction** — token accounting (local estimate anchored by API usage), dual checkpoints (pre-turn and after every tool output), automatic history compaction past the threshold; `/compact` triggers it manually
- **Shell tool** — Git Bash execution, timeout control (120s default / 600s max), live output forwarding, and bounded capture (1 MiB per stream, head+tail with a truncation marker); captured output survives a timeout kill
- **SQLite session persistence** — WAL mode; `/resume` restores sessions, memory survives across processes
- **Extensible kernel** — Provider, Tool, Registry, session-storage, and permission interfaces are in place; permissions currently allow all operations, while complete Skills / Hooks contracts and implementations are planned

## Architecture

```
forge-tui ──► forge-core ──► forge-provider   (chat/completions · responses · messages)
                 │    ╰────► forge-tools      (shell)
                 ╰──────────► forge-storage   (SQLite + SQLx)
```

`forge-core` defines every trait (`ModelProvider` / `Tool` / `AgentEvent` / the compactor / the session-store interface) with zero UI and zero HTTP dependencies. Protocol, tool, and storage implementations live outside core and are assembled through registries. Adding a protocol = implement one trait + register it; the agent loop never changes.

## Platform Requirements

- **Windows** + [Git for Windows](https://gitforwindows.org/) (the shell tool invokes `bash.exe`, auto-detected or configurable)
- Rust stable (edition 2021)

## Quick Start

```bash
git clone https://github.com/yk4464/forge.git
cd forge
cp config.example.toml config.toml   # fill in your base_url / key / model name
cargo run -p forge-tui               # or cargo build --release, then run target/release/forge
```

Provide the API key either inline in `config.toml` (`provider.api_key`, the file is gitignored) or via the environment variable named by `provider.api_key_env`.

Headless mode for scripting/acceptance (consecutive invocations share one session):

```bash
forge check "Summarize the directory structure"
```

## TUI Commands

| Command | Action |
|---------|--------|
| `/new` | Start a new session |
| `/resume` | Resume the most recent session |
| `/compact` | Manually trigger context compaction |
| `/exit` | Quit |
| `/help` | Help |

## Configuration

See [config.example.toml](config.example.toml) — every field is commented: protocol (`openai` / `responses` / `anthropic`), base_url, API key, model, context window size, auto-compaction threshold, shell path and timeout.

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the full feature map, dependencies, implementation order, and acceptance criteria. These stages do not promise calendar dates. v0.0.1 provides the agent loop, TUI, three protocols, SQLite, and basic context compaction; the S0 baseline fixes were completed on 2026-09-19 and work now moves on to S1.

- [x] **S0 Baseline fixes**: shell streaming and memory bounds, error completion, compaction message integrity (completed 2026-09-19)
- [ ] **S1 Runtime and data reliability**: cancellation, original records, crash recovery, task budgets, runtime interfaces
- [ ] **S2 Safe coding workflow**: permissions and workspace boundaries, file tools, project rules, planning and rewind
- [ ] **S3 Daily usability**: Provider recovery, sessions and TUI, context and usage, scripting interface, basic release packages
- [ ] **S4 Extension ecosystem**: Skills, MCP, Hooks, background tasks, and web tools
- [ ] **S5 Multi-agent collaboration**: worktrees, sub-agents, and cross-terminal messaging
- [ ] **S6 Multiple clients**: Web UI (Axum + React) and Tauri 2 desktop app
- [ ] **S7 Optional enhancements**: LSP, PTY, OAuth/subscription quotas, additional platforms, and automatic updates

S5 and S6 can be reordered as needed. Runtime interfaces are introduced in S1; testing and evaluation accompany every stage.

## License

[MIT](LICENSE)
