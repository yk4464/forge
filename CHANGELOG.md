# 更新日志

## v0.1.0 — 2026-09-19

S0 修复基线完成，S1 取消协议开工。72 个测试全绿，CI（windows-latest）通过。
版本策略：`0.MINOR.PATCH`——每完成一个 ROADMAP 阶段 minor +1，阶段间修复 patch +1；
S3「日常可用」验收通过后进入 1.0。

### 缺陷硬化批次（f4b5978..28ea3af，21 项）

- **core**：每轮保证 `Error`/`TurnCompleted` 终态（TUI 不再永久 busy）；权限拒绝事件成对；
  usage 按轮累计并过滤 0 值；上下文超窗一次压缩自愈重试；空 assistant 不入历史
- **provider**：SSE 传输中断不再伪装正常完成；`truncate_body` UTF-8 安全截断；
  connect/read 超时；Anthropic 并行 tool_result 合并、cache token 计入；
  `"error": null` 不再误报
- **tools**：`timeout_ms` 毫秒保真、真实 `duration_ms`、子进程环境脱敏
  （`*API_KEY*`/`*_TOKEN`/`FORGE_*` 不再进入子进程）
- **tui**：`/exit` 真正生效；滚动改为"距底部偏移"语义（unicode-width 精确折行，
  中文不串行）；损坏历史不清库（只读降级 + 明确告警）；会话按项目根隔离

### S0 修复基线（5f7dcba..1c8ed0a）

- **shell**：实时转发并发驱动（select 循环，命令运行中即可见输出）；捕获内存有界
  （每流 1 MiB head+tail + 省略标记，管道始终排空）；实时转发 256 KiB 预算合并；
  超时保留已捕获输出（250ms 短排水）
- **压缩完整性**：system 规则钉住（不再被摘要掉）；尾部未完成工具组钉住；
  溢出 shrink 成组裁剪（不拆散调用/结果对）

### 新能力（00c9a43..f0d6c60，S1 开工）

- **取消协议**：`CancelToken` 贯穿 provider 流与工具执行；取消后历史保持协议合法
  （在执行与未执行的调用补 `cancelled by user` 结果，无悬空 tool_use）；
  TUI `Esc` 取消当前任务（与退出分离），`/exit` 先取消再退出
- **进程树回收**：kill-on-close Job Object——超时/取消/命令结束终止整个进程树
  （含孙进程与后台守护进程），创建失败时回退到仅杀直接子进程

### 已知边界

- 长驻后台进程会随命令结束被回收（显式后台任务管理在 S4）
- 任务预算／循环检测、追加式持久化（原始记录保真）、最小运行接口在 S1 待做
- 权限层当前默认放行（S2）
