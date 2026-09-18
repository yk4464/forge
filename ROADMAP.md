# forge ROADMAP — 未来规划

> 状态标记：`[x]` 已完成（v0.0.1）· `[~]` 部分完成 · `[ ]` 未开始。
> 本文件是功能全景清单，**不排期**；里程碑划分后续另行规划。

## 0. 现状基线（v0.0.1 已有）

- [x] 流式 agent 循环 + 统一 AgentEvent 事件总线
- [x] 三个 SSE 协议面：`/v1/chat/completions`、`/v1/responses`、`/v1/messages`
- [x] shell 工具（Git Bash，超时/截断/实时输出流）
- [x] Codex 风格上下文压缩（pre-turn + 工具输出后双检查点），`/compact` 手动触发
- [x] SQLite 会话持久化（WAL），`/resume`、跨进程记忆、`forge check` 无头模式
- [x] Ratatui TUI 基础（流式正文/思维链/工具卡片、四命令）
- [x] trait 留位：`ModelProvider` / `Tool` / `Skill` / `Hook` / `Permission` + Registry

---

## 1. 工具体系（当前只有 shell，差距最大）

- [ ] **Read / Edit / Write 文件工具**：精确字符串替换、唯一性校验、diff 预览、失败不落盘；token 效率远高于 cat 整个文件
- [ ] **Grep / Glob 搜索工具**：ripgrep 级性能、结构化输出，替代让模型手拼 grep 命令
- [ ] **Todo 工具**：模型可维护任务清单（TodoWrite/TodoRead），TUI 侧栏渲染进度
- [ ] **Task 工具（子代理）**：主 agent 派生子代理执行独立子任务（如全局搜索、批量调研），只回传结论；子代理可指定不同模型
- [ ] **WebSearch / WebFetch**：联网搜索与网页抓取进上下文
- [ ] **后台 / 前台任务**：长命令（dev server、watch）转后台运行，可随时查看输出；前台并行工具调用（模型一次发多个工具调用并发执行）

## 2. 多 agent 协作与跨终端交流

- [ ] **子代理体系**（依赖 Task 工具）：spawn/join、并发上限、结果汇总
- [ ] **跨终端交流**：同机多个 forge 实例互通（本地命名管道/socket 消息总线），一个终端可以直接和另一个终端的 forge 对话、派活、查询状态——类似多 agent 协作；子代理机制复用同一套消息协议

## 3. 权限与安全

- [ ] **权限层实装**：allow / ask / deny 三级策略，按工具名 + 命令前缀/模式匹配；ask 时 TUI 弹确认（本次/本会话/永久）
- [ ] **沙箱（Codex 式）**：参考 openai/codex 的沙箱实现做 Windows 适配——工作区写入边界、网络访问开关、命令审批模式（read-only / auto / full-access）
- [ ] **密钥管理**：系统 keychain 存储，替代明文 config.toml

## 4. 上下文工程

- [ ] **优化上下文缓存**：利用协议面 prompt-caching（OpenAI 自动缓存 / Anthropic `cache_control` 断点），命中省钱省延迟
- [ ] **命中率显示**：缓存命中率的实时展示
- [ ] **上下文进度显示**：TUI 状态栏显示上下文窗口占用（当前已有 token 计数，改为进度条 + 压缩预警）
- [ ] **思考限额**：reasoning effort / thinking budget 可配置、可按会话调整
- [ ] **模型主动提问**：模型可发起 AskUserQuestion 式的选择题向用户对齐，而不是瞎猜继续跑

## 5. 模型接入与 Provider

- [ ] **多模型兼容**：在现有三协议面之上完善各家方言（Kimi、DeepSeek、GLM、Qwen…），模态自选（文本/图片输入）、上下文窗口自选
- [ ] **`/v1/models` 自动获取**：从 base_url 拉取可用模型列表，TUI 内 `/models` 切换
- [ ] **模型回退 + 五次重试**：请求失败指数退避重试（5 次），连续失败按配置的回退链切备用模型
- [ ] **OAuth 登录**：支持厂商 OAuth 订阅登录（Claude Pro/Max、ChatGPT Plus 等），不止 API key
- [ ] **订阅额度显示**：Kimi / Claude / GPT 订阅的 5 小时窗口与周额度展示（社区已有大量开源实现，直接借鉴移植）
- [ ] **用量与价格**：token 用量按天统计（本地 SQLite 记账），按模型单价折算价格显示

## 6. TUI / 交互体验

- [ ] **斜杠命令自动补全**：输入 `/` 弹出命令菜单，模糊过滤，Tab/Enter 选中
- [ ] **diff 高亮**：编辑操作与 git 输出按行着色（增/删/上下文行）
- [ ] **Markdown 渲染**：assistant 输出按 md 渲染（标题/列表/代码块着色/表格）
- [ ] **token 显示 + tps 显示**：状态栏常驻 token 计数与 `xx tok/s` 吞吐速率
- [ ] **任务时间显示**：每轮/每个工具调用的耗时显示
- [ ] **请求中 vs 思考中**：区分"等待 API 响应"与"模型思考中（流式推理）"两种状态动画
- [ ] **命令用途说明**：模型发起 shell 调用时展示一行"这条命令是干什么的"（来自工具参数描述或模型附注），不只是裸命令
- [ ] **对话标题自动生成**：首轮结束后用模型生成短标题，替换会话列表里的默认标题
- [ ] **图片进上下文（剪贴板粘贴）**：TUI 内 Ctrl+V 粘贴剪贴板图片，编码为 base64 进多模态消息（依赖模态自选的 Provider 扩展）
- [ ] **回退（rewind）**：会话内消息级回退 + 配合 git 检查点回退代码

## 7. Git 集成

- [ ] **检查点 / 回退基础**：任务开始前自动打 git 检查点（stash/branch），`/rewind` 可回退代码 + 会话到任意检查点
- [ ] **worktree 隔离**：并行/子代理任务各自在独立 git worktree 中工作，互不踩踏
- [ ] **git 使用规范**：系统提示词层约束——规范提交信息、分批提交、不擅自 push
- [ ] **commit / PR 辅助**：自动生成提交信息、审查未提交变更、起草 PR 描述

## 8. 项目记忆与系统提示词工程（十分重要）

- [ ] **AGENTS.md 自动读取**：启动时按 全局 → 项目根 → 子目录 分层加载 AGENTS.md，拼入 system prompt
- [ ] **`/init` 命令**：扫描代码库自动生成项目 AGENTS.md（结构、构建命令、约定）
- [ ] **系统提示词工程**：全面重写与打磨 system prompt——工具使用策略、多步规划习惯、错误恢复、输出风格、安全边界；这是效果提升最大的单项投入
- [ ] **计划模式（plan mode）**：先探索出计划、用户批准后再动手执行的只读模式

## 9. MCP 与 Skills

- [ ] **MCP client（rmcp）**：接入 MCP server 生态，MCP 工具注册进统一 Tool Registry；`mcp add` 配置式管理
- [ ] **Skills 实现**：目录/配置式 skill（SKILL.md + 资源），按触发词或 `/skill` 调用
- [ ] **内置 skills 与内置 MCP**：随发行版带一组开箱可用的 skills 和推荐 MCP 配置

## 10. Hooks

- [ ] **Hooks 实装**：before/after agent、before/after model、before/after tool、on_error 等生命周期钩子，支持用户配置的命令/脚本，可阻断或改写

## 11. 杂项命令

- [ ] **`/btw` 命令**：临时侧问——开一个不写主会话历史、不占上下文的旁路对话，答完即弃
- [ ] **md 格式兼容**：输入输出对 Markdown 的完整兼容（渲染 + 落库保真）
- [ ] **跨终端交流协议公开化**：消息总线协议文档化，第三方可接入

---

## 附：与成熟 agent（Claude Code / OpenCode / Codex）的主要差距主题

工具广度（1）· 安全边界（3）· 项目记忆与提示词（8）· MCP 生态（9）——这四块补齐后与生产级 agent 功能面基本对齐。
