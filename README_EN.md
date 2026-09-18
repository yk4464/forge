# forge

⚡ A streaming-first terminal coding agent written in Rust (Milestone 1).

English | [中文](README.md)

forge runs in your terminal: you give instructions in natural language, it executes commands through a single `shell` tool (read/write files, search, build, run tests…), streams its reasoning and output live, and keeps going until the task is done. When the context approaches the model's window, it auto-compacts (Codex-style) so long sessions keep working.

## Features

- **Streaming-first** — assistant text, chain-of-thought, and tool output all flow through a unified `AgentEvent` bus; the Ratatui TUI renders them as they arrive
- **Three wire protocols, one trait** — `/v1/chat/completions` (OpenAI-compatible: DeepSeek, Zhipu, Qwen, …), `/v1/responses`, and `/v1/messages` (Anthropic format); all SSE-streamed, with `reasoning_content` / `thinking` surfaced as separate events
- **Codex-style context compaction** — token accounting (local estimate anchored by API usage), dual checkpoints (pre-turn and after every tool output), automatic history compaction past the threshold; `/compact` triggers it manually
- **One shell tool to rule them all** — commands run via Git Bash with live output streaming, timeout control (120s default / 600s max), and a 1 MiB capture cap with middle truncation
- **SQLite session persistence** — WAL mode; `/resume` restores sessions, memory survives across processes
- **Extensible kernel** — `ModelProvider` / `Tool` / `Skill` / `Hook` / `Permission` are all traits behind registries; the permission layer is allow-all for now, with the architecture in place

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

See [ROADMAP.md](ROADMAP.md) for the full feature map (unscheduled). Overview:

- [x] M1: agent loop + TUI + three protocols + SQLite + context compaction
- [ ] File edit/search tools, permissions & sandbox, MCP, Skills, Hooks
- [ ] Sub-agents & cross-terminal collaboration, image input, model fallback/OAuth/usage stats
- [ ] Web UI (Axum + React), Tauri 2 desktop app

## License

[MIT](LICENSE)
