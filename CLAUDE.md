@AGENTS.md

## ⚠️ 关键排障文档（后端 / aionrs 相关，新 AI 首读）

> **三仓 fork 对上游的映射、版本对照、当前同步状态、同步套路与不变量见：**[`../1oneUI/docs/guides/upstream-sync-reference.zh-CN.md`](../1oneUI/docs/guides/upstream-sync-reference.zh-CN.md)

- **【2026-07-20 最新】企业组织 vs 项目组彻底解耦（新增 one-enterprise crate，本仓 one-org/one-sso/aionui-app 均有改动）**：⚠️ **下面 07-16 条目描述的 `EnterpriseAutoJoiner`/`auto_provision_enterprise`/`one_tenants.sso_provider` 架构已被本次彻底取代**——SSO 公司维度独立到新 crate `one-enterprise`（`one_enterprises`/`one_enterprise_members`），`one_tenants` 回归纯邀请码项目组，trait 改名 `EnterpriseSync::sync_member`。真实开发库（非 `:memory:`）迁移冒烟已验证 `DROP COLUMN` 迁移可行：
  [`../1oneUI/docs/guides/session-2026-07-20-enterprise-org-decouple.zh-CN.md`](../1oneUI/docs/guides/session-2026-07-20-enterprise-org-decouple.zh-CN.md)
- **【2026-07-16】飞书桌面登录根治 + 「真实企业」层(B2，本仓 one-sso/one-org/aionui-app 均有改动，⚠️架构已被 07-20 取代，见上条)**：
  [`../1oneUI/docs/guides/session-2026-07-16-enterprise-tier-and-sso-fixes.zh-CN.md`](../1oneUI/docs/guides/session-2026-07-16-enterprise-tier-and-sso-fixes.zh-CN.md)
  - 一句话：桌面 SSO 登录走不完最后一步 = `one-sso` 的 `desktop_callback_page` 里 **`setTimeout(window.close, 1200)` 抢掉了浏览器的协议确认框**（与 scheme/平台无关），已改 5 秒；deep link 补传 `name`（服务器有「赵高」但只传了被 sanitize 成 `sso_xxx` 的 username）。
  - 新增「真实企业」层：`ProviderUserInfo.org_external_id`(飞书 `tenant_key`) → `create_tenant` 绑公司 → `OrgService::auto_provision_enterprise` 同公司 SSO 登录自动入伙；经 `one_sso::EnterpriseAutoJoiner` trait + `aionui-app` 的 `OrgEnterpriseAutoJoiner` adapter 接线（同层 crate 只能靠 trait）。**callback 必须在 `issue_session` 之前调用**（auto-join 会轮换 jwt secret）。
  - **⚠️ 红线**：`auto_provision_enterprise` **join-only、永不建 tenant**（否则单机装机会被悄悄变成企业服务器），`auto_joiner` 为 `Option`；有专门测试 `auto_provision_enterprise_never_creates_a_tenant` 锁死。
  - 新增两个迁移账本条目：`one-org/004_tenant_sso_binding`、`one-sso/004_identity_org_external_id`。**改完必须 `backend-rebuild.ps1` 重编进 bundled 才生效**。
- **aionrs OpenAI 协议 thinking 参数 / 网关拒绝 tool_calls / 文本化工具历史兜底 / 授权模式默认全自动**（2026-07-10~11）：
  完整分析、三仓 commit 索引、黑盒探测网关方法论、上游对齐（issue #74 / PR #203）都在 1oneUI 仓库的这份 session 文档：
  [`../1oneUI/docs/guides/session-2026-07-10-thinking-param-and-rename.zh-CN.md`](../1oneUI/docs/guides/session-2026-07-10-thinking-param-and-rename.zh-CN.md)
  - 一句话：deepseek 等模型 Agent 任务报 `content[].thinking must be passed back` = 网关 `litellm-internal.123u.com` 的 DeepSeek 渠道**无状态拒绝一切 tool_calls 历史**（客户端无法用请求格式满足），aionrs fork 已用「文本化工具历史」绕过（commit `1f36350`）。
  - `session_mode` 默认 `yolo`（授权全自动）的后端兜底在 `aionui-conversation/src/service.rs` 的 `resolve_assistant_snapshot`（仅对从未用过、`default_permission_mode==auto` 的 aionrs 助手生效）。
- **改完 Rust 必须重编** `aioncore.exe` 并搬进 1oneUI bundled 才生效（详见 `../1oneUI/docs/guides/ai-handoff-conventions.zh-CN.md`）。
