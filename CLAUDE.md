@AGENTS.md

## ⚠️ 关键排障文档（后端 / aionrs 相关，新 AI 首读）

> aionrs 依赖来自 fork **`gaogg521/aionrs`** 的 **`master`** 分支（见本仓 `Cargo.toml` 的
> `aion-*` git 依赖）；本地源码在同级目录 `../aionrs-local`。上游 = `iOfficeAI/aionrs`，**只单向同步上游 → fork，不反向提 PR**。
> （历史遗留的 `fix-openai-thinking-param` 功能分支已快进合并进 master 并删除，改动全在 master。）

- **aionrs OpenAI 协议 thinking 参数 / 网关拒绝 tool_calls / 文本化工具历史兜底 / 授权模式默认全自动**（2026-07-10~11）：
  完整分析、三仓 commit 索引、黑盒探测网关方法论、上游对齐（issue #74 / PR #203）都在 1oneUI 仓库的这份 session 文档：
  [`../1oneUI/docs/guides/session-2026-07-10-thinking-param-and-rename.zh-CN.md`](../1oneUI/docs/guides/session-2026-07-10-thinking-param-and-rename.zh-CN.md)
  - 一句话：deepseek 等模型 Agent 任务报 `content[].thinking must be passed back` = 网关 `litellm-internal.123u.com` 的 DeepSeek 渠道**无状态拒绝一切 tool_calls 历史**（客户端无法用请求格式满足），aionrs fork 已用「文本化工具历史」绕过（commit `1f36350`）。
  - `session_mode` 默认 `yolo`（授权全自动）的后端兜底在 `aionui-conversation/src/service.rs` 的 `resolve_assistant_snapshot`（仅对从未用过、`default_permission_mode==auto` 的 aionrs 助手生效）。
- **改完 Rust 必须重编** `aioncore.exe` 并搬进 1oneUI bundled 才生效（详见 `../1oneUI/docs/guides/ai-handoff-conventions.zh-CN.md`）。
- **上游对照与同步参考**（本仓 = fork 自 `iOfficeAI/AionCore`；版本对照、当前落后多少、后端待同步项、同步套路与不变量）：[`../1oneUI/docs/guides/upstream-sync-reference.zh-CN.md`](../1oneUI/docs/guides/upstream-sync-reference.zh-CN.md)
  - ⚠️ **当前后端未同步**：停在 **v0.1.42**，上游已 **v0.1.45**（落后 31 commit / 3 版本）。其中 v0.1.44 #585 是前端反馈诊断端点的后端实现，未同步 = 该功能静默降级。
