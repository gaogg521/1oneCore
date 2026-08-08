//! Storage and distribution for content-inspection rules and their findings.
//!
//! The scanning itself is `aionui_common::dlp` — a pure function with no
//! database and no policy. This module is the enterprise half: who may author a
//! rule, which rules a given member is subject to, and where findings land.
//!
//! # Why members can read rules but only admins can write them
//!
//! Enforcement runs on the member's own machine (see the migration for why), so
//! a member's client necessarily fetches the rules that apply to it. That is a
//! real, accepted exposure: a pattern can itself be sensitive. What members
//! cannot do is change one — authoring is admin-only, and a member editing their
//! own copy locally is the same trust boundary every desktop product has.

use aionui_common::dlp::{DlpAction, DlpMatcher, DlpRule, builtin_spec};
use aionui_common::now_ms;
use serde::Serialize;
use sqlx::FromRow;

use crate::error::DevopsError;
use crate::service::DevopsService;

const RULE_COLS: &str =
    "id, name, matcher, pattern, action, enabled, scope, team_id, created_by, created_at, updated_at";

/// A rule as the admin console sees it.
#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DlpRuleDto {
    pub id: String,
    pub name: String,
    pub matcher: String,
    pub pattern: String,
    pub action: String,
    pub enabled: bool,
    pub scope: String,
    pub team_id: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One recorded finding, for the admin's review screen.
#[derive(Debug, Clone, FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DlpEventDto {
    pub id: String,
    pub user_id: String,
    pub conversation_id: Option<String>,
    pub model: Option<String>,
    pub rule_id: String,
    pub rule_name: String,
    pub action: String,
    pub hits: i64,
    /// Context with the matched value masked — never the value itself.
    pub excerpt: String,
    pub team_id: Option<String>,
    pub created_at: i64,
}

/// Everything needed to create or update one rule.
///
/// A struct rather than positional parameters because six of these are `&str`:
/// swapping `name` and `pattern` compiles cleanly and silently stores a rule
/// that matches its own title.
pub struct UpsertDlpRule<'a> {
    /// `None` creates; `Some` updates and 404s if the rule is gone.
    pub id: Option<&'a str>,
    pub name: &'a str,
    pub matcher: &'a str,
    pub pattern: &'a str,
    pub action: &'a str,
    pub enabled: bool,
    pub scope: &'a str,
    pub team_id: Option<&'a str>,
    pub created_by: &'a str,
}

/// One finding being reported by a member's client.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DlpEventInput {
    pub conversation_id: Option<String>,
    pub model: Option<String>,
    pub rule_id: String,
    pub rule_name: String,
    pub action: String,
    #[serde(default = "one")]
    pub hits: i64,
    #[serde(default)]
    pub excerpt: String,
}

fn one() -> i64 {
    1
}

fn parse_matcher(raw: &str) -> DlpMatcher {
    match raw {
        "regex" => DlpMatcher::Regex,
        "builtin" => DlpMatcher::Builtin,
        _ => DlpMatcher::Keyword,
    }
}

fn parse_action(raw: &str) -> DlpAction {
    match raw {
        "block" => DlpAction::Block,
        _ => DlpAction::Log,
    }
}

impl DlpRuleDto {
    /// Convert to the shape the scanner takes.
    pub fn to_rule(&self) -> DlpRule {
        DlpRule {
            id: self.id.clone(),
            name: self.name.clone(),
            matcher: parse_matcher(&self.matcher),
            pattern: self.pattern.clone(),
            action: parse_action(&self.action),
        }
    }
}

impl DevopsService {
    /// Every rule, for the admin console. Includes disabled ones — an admin
    /// managing rules needs to see the ones they switched off.
    pub async fn list_dlp_rules(&self) -> Result<Vec<DlpRuleDto>, DevopsError> {
        Ok(sqlx::query_as::<_, DlpRuleDto>(&format!(
            "SELECT {RULE_COLS} FROM one_dlp_rules ORDER BY updated_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    /// The rules a given member is actually subject to: enabled, and either
    /// company-wide or bound to a project group they belong to.
    ///
    /// ⚠️ Deliberately **not** filtered by the `visibility` column the other
    /// registries use. A rule the client cannot read is a rule the client cannot
    /// enforce, so an "admin-only" DLP rule would be one that silently does
    /// nothing — the worst possible outcome for a control a company is relying
    /// on.
    pub async fn list_dlp_rules_for_member(&self, viewer_user_id: &str) -> Result<Vec<DlpRuleDto>, DevopsError> {
        let sql = format!(
            "SELECT {RULE_COLS} FROM one_dlp_rules \
             WHERE enabled = 1 AND (scope = 'org' OR (scope = 'team' AND team_id IN \
               (SELECT tenant_id FROM one_user_org WHERE user_id = ?))) \
             ORDER BY updated_at DESC"
        );
        Ok(sqlx::query_as::<_, DlpRuleDto>(&sql)
            .bind(viewer_user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn upsert_dlp_rule(&self, input: UpsertDlpRule<'_>) -> Result<DlpRuleDto, DevopsError> {
        let UpsertDlpRule {
            id,
            name,
            matcher,
            pattern,
            action,
            enabled,
            scope,
            team_id,
            created_by,
        } = input;
        let name = name.trim();
        if name.is_empty() {
            return Err(DevopsError::BadRequest("name is required".into()));
        }
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(DevopsError::BadRequest("pattern is required".into()));
        }
        if !matches!(matcher, "keyword" | "regex" | "builtin") {
            return Err(DevopsError::BadRequest(format!("invalid matcher: {matcher}")));
        }
        if !matches!(action, "log" | "block") {
            return Err(DevopsError::BadRequest(format!("invalid action: {action}")));
        }

        // Validate at authoring time rather than letting it fail silently on
        // every member's machine. A rule that cannot compile enforces nothing,
        // and looks exactly like a rule that is working.
        match matcher {
            "regex" => {
                regex::Regex::new(pattern).map_err(|e| {
                    DevopsError::BadRequest(format!(
                        "this pattern is not a usable regular expression: {e}. \
                         Note that look-around (?=, ?!, ?<=, ?<!) is not supported."
                    ))
                })?;
            }
            "builtin" if builtin_spec(pattern).is_none() => {
                return Err(DevopsError::BadRequest(format!("unknown built-in pattern: {pattern}")));
            }
            _ => {}
        }

        // `visibility` is not a dimension for rules (see
        // `list_dlp_rules_for_member`), so validate the scope with the shared
        // helper's own default for it.
        let team_id = self.validate_resource_scope(created_by, scope, team_id, "all").await?;

        let now = now_ms();
        let id = match id {
            Some(existing) => {
                let updated = sqlx::query(
                    "UPDATE one_dlp_rules SET name = ?, matcher = ?, pattern = ?, action = ?, enabled = ?, \
                     scope = ?, team_id = ?, updated_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(matcher)
                .bind(pattern)
                .bind(action)
                .bind(enabled)
                .bind(scope)
                .bind(team_id)
                .bind(now)
                .bind(existing)
                .execute(&self.pool)
                .await?;
                if updated.rows_affected() == 0 {
                    return Err(DevopsError::NotFound(format!("dlp rule {existing}")));
                }
                existing.to_owned()
            }
            None => {
                let id = format!("odlp_{}", uuid::Uuid::now_v7().simple());
                sqlx::query(
                    "INSERT INTO one_dlp_rules \
                        (id, name, matcher, pattern, action, enabled, scope, team_id, created_by, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(name)
                .bind(matcher)
                .bind(pattern)
                .bind(action)
                .bind(enabled)
                .bind(scope)
                .bind(team_id)
                .bind(created_by)
                .bind(now)
                .bind(now)
                .execute(&self.pool)
                .await?;
                id
            }
        };

        sqlx::query_as::<_, DlpRuleDto>(&format!("SELECT {RULE_COLS} FROM one_dlp_rules WHERE id = ?"))
            .bind(&id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn delete_dlp_rule(&self, id: &str) -> Result<(), DevopsError> {
        let deleted = sqlx::query("DELETE FROM one_dlp_rules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if deleted.rows_affected() == 0 {
            return Err(DevopsError::NotFound(format!("dlp rule {id}")));
        }
        Ok(())
    }

    /// Record findings a member's client reported.
    ///
    /// Best-effort by contract: the send already happened (or was already
    /// blocked locally), so failing the report would change nothing about the
    /// content and only lose the record.
    pub async fn record_dlp_events(&self, user_id: &str, events: &[DlpEventInput]) -> Result<u64, DevopsError> {
        if events.is_empty() {
            return Ok(0);
        }
        // Which project group the member was acting in, so a reviewer can scope
        // by team the same way the rest of the console does.
        let team_id: Option<String> =
            sqlx::query_scalar("SELECT tenant_id FROM one_user_org WHERE user_id = ? LIMIT 1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or(None);

        let now = now_ms();
        let mut written = 0u64;
        for event in events {
            let result = sqlx::query(
                "INSERT INTO one_dlp_events \
                    (id, user_id, conversation_id, model, rule_id, rule_name, action, hits, excerpt, team_id, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("odlpe_{}", uuid::Uuid::now_v7().simple()))
            .bind(user_id)
            .bind(event.conversation_id.as_deref())
            .bind(event.model.as_deref())
            .bind(&event.rule_id)
            .bind(&event.rule_name)
            .bind(&event.action)
            .bind(event.hits.max(1))
            .bind(&event.excerpt)
            .bind(team_id.as_deref())
            .bind(now)
            .execute(&self.pool)
            .await?;
            written += result.rows_affected();
        }
        Ok(written)
    }

    /// Recorded findings, newest first, for the admin review screen.
    pub async fn list_dlp_events(&self, limit: i64) -> Result<Vec<DlpEventDto>, DevopsError> {
        let limit = limit.clamp(1, 500);
        Ok(sqlx::query_as::<_, DlpEventDto>(
            "SELECT id, user_id, conversation_id, model, rule_id, rule_name, action, hits, excerpt, team_id, created_at \
             FROM one_dlp_events ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::run_one_devops_migrations;

    async fn service() -> DevopsService {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        run_one_devops_migrations(&pool).await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE one_tenants (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));
             CREATE TABLE one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, updated_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO one_tenants (id, name) VALUES ('tA', 'Group A'), ('tB', 'Group B');
             INSERT INTO one_user_org (user_id, tenant_id, role) VALUES ('admin1', 'tA', 'org_admin'), ('memberA', 'tA', 'member'), ('memberB', 'tB', 'member');
             INSERT INTO one_active_tenant (user_id, tenant_id) VALUES ('admin1', 'tA'), ('memberA', 'tA'), ('memberB', 'tB');",
        )
        .execute(&pool)
        .await
        .unwrap();
        DevopsService::new(pool)
    }

    async fn add(
        svc: &DevopsService,
        name: &str,
        matcher: &str,
        pattern: &str,
        scope: &str,
        team: Option<&str>,
    ) -> DlpRuleDto {
        svc.upsert_dlp_rule(UpsertDlpRule {
            id: None,
            name,
            matcher,
            pattern,
            action: "log",
            enabled: true,
            scope,
            team_id: team,
            created_by: "admin1",
        })
        .await
        .unwrap()
    }

    /// A rule that cannot compile enforces nothing while looking exactly like a
    /// rule that works. Catching it at authoring time is the only moment an
    /// admin is present to fix it.
    #[tokio::test]
    async fn an_uncompilable_pattern_is_refused_when_it_is_written() {
        let svc = service().await;
        let err = svc
            .upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "bad",
                matcher: "regex",
                pattern: "([unclosed",
                action: "log",
                enabled: true,
                scope: "org",
                team_id: None,
                created_by: "admin1",
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DevopsError::BadRequest(_)), "got {err:?}");
        assert!(
            svc.list_dlp_rules().await.unwrap().is_empty(),
            "a broken rule was stored"
        );
    }

    /// Look-around is the mistake an admin who knows regex will actually make,
    /// because it works everywhere else. Say so instead of "invalid".
    #[tokio::test]
    async fn a_lookaround_pattern_is_refused_with_an_explanation() {
        let svc = service().await;
        let err = svc
            .upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "la",
                matcher: "regex",
                pattern: r"(?<![0-9])123",
                action: "log",
                enabled: true,
                scope: "org",
                team_id: None,
                created_by: "admin1",
            })
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("look-around"), "unhelpful message: {message}");
    }

    #[tokio::test]
    async fn an_unknown_builtin_is_refused() {
        let svc = service().await;
        assert!(
            svc.upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "b",
                matcher: "builtin",
                pattern: "no_such",
                action: "log",
                enabled: true,
                scope: "org",
                team_id: None,
                created_by: "admin1",
            })
            .await
            .is_err()
        );
        // …and a real one is accepted.
        assert!(
            svc.upsert_dlp_rule(UpsertDlpRule {
                id: None,
                name: "b",
                matcher: "builtin",
                pattern: "cn_id_card",
                action: "log",
                enabled: true,
                scope: "org",
                team_id: None,
                created_by: "admin1",
            })
            .await
            .is_ok()
        );
    }

    /// Rules follow project-group scoping like every other distributed resource.
    #[tokio::test]
    async fn a_member_gets_company_rules_plus_their_own_groups() {
        let svc = service().await;
        add(&svc, "company-wide", "keyword", "secret", "org", None).await;
        add(&svc, "group-a-only", "keyword", "bluebird", "team", Some("tA")).await;

        let names = |rows: Vec<DlpRuleDto>| -> Vec<String> { rows.into_iter().map(|r| r.name).collect() };
        let a = names(svc.list_dlp_rules_for_member("memberA").await.unwrap());
        assert!(a.contains(&"company-wide".to_owned()) && a.contains(&"group-a-only".to_owned()));

        let b = names(svc.list_dlp_rules_for_member("memberB").await.unwrap());
        assert!(b.contains(&"company-wide".to_owned()));
        assert!(!b.contains(&"group-a-only".to_owned()), "another group's rule leaked");
    }

    /// A disabled rule must not reach a member — otherwise switching a rule off
    /// in the console would not switch it off in practice.
    #[tokio::test]
    async fn a_disabled_rule_is_not_distributed_but_stays_visible_to_admins() {
        let svc = service().await;
        let rule = add(&svc, "off", "keyword", "secret", "org", None).await;
        svc.upsert_dlp_rule(UpsertDlpRule {
            id: Some(&rule.id),
            name: "off",
            matcher: "keyword",
            pattern: "secret",
            action: "log",
            enabled: false,
            scope: "org",
            team_id: None,
            created_by: "admin1",
        })
        .await
        .unwrap();

        assert!(svc.list_dlp_rules_for_member("memberA").await.unwrap().is_empty());
        assert_eq!(svc.list_dlp_rules().await.unwrap().len(), 1, "admin still sees it");
    }

    #[tokio::test]
    async fn findings_are_recorded_and_attributed_to_the_members_group() {
        let svc = service().await;
        let written = svc
            .record_dlp_events(
                "memberA",
                &[DlpEventInput {
                    conversation_id: Some("conv1".into()),
                    model: Some("gpt-4".into()),
                    rule_id: "r1".into(),
                    rule_name: "客户名".into(),
                    action: "log".into(),
                    hits: 3,
                    excerpt: "…客户是 **** 请注意…".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(written, 1);

        let events = svc.list_dlp_events(50).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].user_id, "memberA");
        assert_eq!(events[0].team_id.as_deref(), Some("tA"));
        assert_eq!(events[0].hits, 3);
    }

    /// The rule name is denormalised so a finding still reads correctly after
    /// the rule it came from is deleted — otherwise last quarter's audit turns
    /// into a list of opaque ids.
    #[tokio::test]
    async fn a_finding_survives_its_rule_being_deleted() {
        let svc = service().await;
        let rule = add(&svc, "客户名单", "keyword", "acme", "org", None).await;
        svc.record_dlp_events(
            "memberA",
            &[DlpEventInput {
                conversation_id: None,
                model: None,
                rule_id: rule.id.clone(),
                rule_name: rule.name.clone(),
                action: "log".into(),
                hits: 1,
                excerpt: "…**…".into(),
            }],
        )
        .await
        .unwrap();

        svc.delete_dlp_rule(&rule.id).await.unwrap();

        let events = svc.list_dlp_events(50).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rule_name, "客户名单");
    }

    #[tokio::test]
    async fn recording_nothing_is_not_an_error() {
        let svc = service().await;
        assert_eq!(svc.record_dlp_events("memberA", &[]).await.unwrap(), 0);
    }
}
