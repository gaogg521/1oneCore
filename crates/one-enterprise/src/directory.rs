//! The company directory mirror (T6) and the departure signal derived from it.
//!
//! # What this is not
//!
//! It is not membership and it is not seats. `one_enterprise_members` is the
//! seat table — billing counts rows in it — and it only ever holds people who
//! have signed in. This mirror holds what the IdP says exists, most of whom
//! will never log in. Keeping them apart is the difference between "sync the
//! directory" and "bill for the whole company the first time sync runs".
//!
//! # The rule that makes this safe
//!
//! Absence from the directory is how somebody gets flagged as having left, and
//! acting on that flag removes their access, rotates their tokens and hands
//! their resources to someone else. So absence has to mean *absence*, not
//! *we didn't manage to fetch them*.
//!
//! [`DirectorySyncInput::complete`] carries that distinction from the fetch
//! side, and [`EnterpriseService::apply_directory_snapshot`] refuses to draw any
//! departure conclusion when it is `false`. An incomplete pull still refreshes
//! what it did see — that data is real — but it cannot mark anyone missing and
//! it cannot clear anyone's existing missing flag either.
//!
//! Same shape as the `authoritative` flag on the team skill / MCP /
//! model-channel syncs, for the same reason.

use aionui_common::now_ms;

use crate::error::EnterpriseError;
use crate::service::EnterpriseService;

/// One department, as handed over by the fetch side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryDepartmentInput {
    pub external_id: String,
    pub parent_external_id: Option<String>,
    pub name: String,
}

/// One person, as handed over by the fetch side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPersonInput {
    pub external_id: String,
    pub name: Option<String>,
    pub job_title: Option<String>,
    pub department_external_id: Option<String>,
    /// `false` when the IdP flags them as resigned.
    pub active: bool,
}

/// A whole directory pull, ready to be reconciled into the mirror.
#[derive(Debug, Clone)]
pub struct DirectorySyncInput {
    pub provider: String,
    pub external_id_field: String,
    pub departments: Vec<DirectoryDepartmentInput>,
    pub people: Vec<DirectoryPersonInput>,
    /// ⚠️ See the module docs. `false` forbids every departure conclusion.
    pub complete: bool,
    pub error: Option<String>,
}

/// What one reconcile did, for the admin console and the logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorySyncReport {
    pub departments: usize,
    pub people: usize,
    /// Newly flagged as gone this run. Always 0 for an incomplete pull.
    pub newly_missing: usize,
    /// Previously flagged, seen again this run. Always 0 for an incomplete pull.
    pub returned: usize,
    pub complete: bool,
}

/// A project group the departed member still belongs to.
///
/// Offboarding is project-group scoped — `OrgService::remove_member` acts on
/// the caller's *active* group and refuses a user who is not in it, and a
/// resource hand-over additionally requires the recipient to be in that same
/// group. So the console cannot offer "remove" until it knows which groups the
/// person is actually in; without this it would fire a call that fails, leave
/// the seat occupied, and report a raw backend error.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepartedTenantRef {
    pub tenant_id: String,
    /// `None` only if the group row vanished under the membership.
    pub name: Option<String>,
}

/// A person the directory no longer vouches for, who still holds a company
/// membership here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepartedMemberDto {
    pub user_id: String,
    pub external_id: String,
    pub display_name: Option<String>,
    pub department: Option<String>,
    pub missing_since: i64,
    /// Project groups this person is still in. Empty is a real answer: a
    /// company member who never joined a group only needs the company-level
    /// removal.
    pub tenants: Vec<DepartedTenantRef>,
}

impl EnterpriseService {
    /// Write a pull into the mirror and, when it is complete, update who is
    /// missing.
    pub async fn apply_directory_snapshot(
        &self,
        enterprise_id: &str,
        input: &DirectorySyncInput,
    ) -> Result<DirectorySyncReport, EnterpriseError> {
        let pool = self.pool_ref();
        let now = now_ms() as i64;
        let mut tx = pool.begin().await?;

        for department in &input.departments {
            sqlx::query(
                "INSERT INTO one_directory_departments \
                   (enterprise_id, external_id, parent_external_id, name, first_seen_at, last_seen_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(enterprise_id, external_id) DO UPDATE SET \
                   parent_external_id = excluded.parent_external_id, \
                   name = excluded.name, \
                   last_seen_at = excluded.last_seen_at",
            )
            .bind(enterprise_id)
            .bind(&department.external_id)
            .bind(department.parent_external_id.as_deref())
            .bind(&department.name)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        for person in &input.people {
            sqlx::query(
                "INSERT INTO one_directory_people \
                   (enterprise_id, external_id, name, job_title, department_external_id, active, \
                    first_seen_at, last_seen_at, missing_since) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL) \
                 ON CONFLICT(enterprise_id, external_id) DO UPDATE SET \
                   name = excluded.name, \
                   job_title = excluded.job_title, \
                   department_external_id = excluded.department_external_id, \
                   active = excluded.active, \
                   last_seen_at = excluded.last_seen_at",
            )
            .bind(enterprise_id)
            .bind(&person.external_id)
            .bind(person.name.as_deref())
            .bind(person.job_title.as_deref())
            .bind(person.department_external_id.as_deref())
            .bind(i64::from(person.active))
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        let mut report = DirectorySyncReport {
            departments: input.departments.len(),
            people: input.people.len(),
            complete: input.complete,
            ..Default::default()
        };

        if input.complete {
            // Anyone not touched by this run is genuinely absent — the pull saw
            // the whole directory. `last_seen_at < now` is the marker; it is
            // exact because every row above was stamped with this same `now`.
            let newly_missing = sqlx::query(
                "UPDATE one_directory_people SET missing_since = ? \
                 WHERE enterprise_id = ? AND missing_since IS NULL \
                   AND (last_seen_at < ? OR active = 0)",
            )
            .bind(now)
            .bind(enterprise_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;

            // Somebody who came back (rehired, un-resigned, or a fixed IdP
            // record) must lose the flag, or the console keeps proposing to
            // offboard a current employee.
            let returned = sqlx::query(
                "UPDATE one_directory_people SET missing_since = NULL \
                 WHERE enterprise_id = ? AND missing_since IS NOT NULL \
                   AND last_seen_at = ? AND active = 1",
            )
            .bind(enterprise_id)
            .bind(now)
            .execute(&mut *tx)
            .await?;

            report.newly_missing = newly_missing.rows_affected() as usize;
            report.returned = returned.rows_affected() as usize;
        }

        let status = if input.complete { "ok" } else { "partial" };
        sqlx::query(
            "INSERT INTO one_directory_sync_state \
               (enterprise_id, provider, external_id_field, last_run_at, last_status, last_error, \
                department_count, people_count, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(enterprise_id) DO UPDATE SET \
               provider = excluded.provider, \
               external_id_field = excluded.external_id_field, \
               last_run_at = excluded.last_run_at, \
               last_status = excluded.last_status, \
               last_error = excluded.last_error, \
               department_count = excluded.department_count, \
               people_count = excluded.people_count, \
               updated_at = excluded.updated_at",
        )
        .bind(enterprise_id)
        .bind(&input.provider)
        .bind(&input.external_id_field)
        .bind(now)
        .bind(status)
        .bind(input.error.as_deref())
        .bind(input.departments.len() as i64)
        .bind(input.people.len() as i64)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(report)
    }

    /// Company members the directory no longer vouches for.
    ///
    /// The join is `one_sso_identities`: a mirror row is matched to a local
    /// account by `(provider, external_id)`, which is exactly what the login
    /// path binds. Somebody with no identity row cannot be matched and is
    /// therefore never proposed — a locally-created account is not a Feishu
    /// employee who left.
    pub async fn list_departed_members(&self, enterprise_id: &str) -> Result<Vec<DepartedMemberDto>, EnterpriseError> {
        let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>, i64)>(
            "SELECT m.user_id, p.external_id, m.display_name, m.department, p.missing_since \
             FROM one_directory_people p \
             JOIN one_sso_identities i \
               ON i.external_id = p.external_id \
             JOIN one_enterprise_members m \
               ON m.user_id = i.user_id AND m.enterprise_id = p.enterprise_id \
             WHERE p.enterprise_id = ? AND p.missing_since IS NOT NULL \
             ORDER BY p.missing_since DESC",
        )
        .bind(enterprise_id)
        .fetch_all(self.pool_ref())
        .await?;

        let mut members: Vec<DepartedMemberDto> = rows
            .into_iter()
            .map(
                |(user_id, external_id, display_name, department, missing_since)| DepartedMemberDto {
                    user_id,
                    external_id,
                    display_name,
                    department,
                    missing_since,
                    tenants: Vec::new(),
                },
            )
            .collect();

        let mut groups = self
            .project_groups_of(&members.iter().map(|m| m.user_id.clone()).collect::<Vec<_>>())
            .await?;
        for member in &mut members {
            member.tenants = groups.remove(&member.user_id).unwrap_or_default();
        }
        Ok(members)
    }

    /// Project-group memberships for the given users, keyed by user id.
    ///
    /// `one_user_org` / `one_tenants` belong to one-org. A deployment that has
    /// no project groups at all — standalone/personal, or a test that only ran
    /// this crate's migrations — simply has nothing to report here, which is an
    /// empty answer rather than a failure: the departed list must still render.
    async fn project_groups_of(
        &self,
        user_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<DepartedTenantRef>>, EnterpriseError> {
        let mut out: std::collections::HashMap<String, Vec<DepartedTenantRef>> = std::collections::HashMap::new();
        if user_ids.is_empty() {
            return Ok(out);
        }
        let placeholders = vec!["?"; user_ids.len()].join(", ");
        let sql = format!(
            "SELECT o.user_id, o.tenant_id, t.name \
             FROM one_user_org o \
             LEFT JOIN one_tenants t ON t.id = o.tenant_id \
             WHERE o.user_id IN ({placeholders}) \
             ORDER BY o.user_id, t.name, o.tenant_id"
        );
        let mut query = sqlx::query_as::<_, (String, String, Option<String>)>(&sql);
        for user_id in user_ids {
            query = query.bind(user_id);
        }
        let rows = match query.fetch_all(self.pool_ref()).await {
            Ok(rows) => rows,
            Err(sqlx::Error::Database(e)) if e.message().contains("no such table") => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for (user_id, tenant_id, name) in rows {
            out.entry(user_id)
                .or_default()
                .push(DepartedTenantRef { tenant_id, name });
        }
        Ok(out)
    }

    /// Last-run status for the admin console.
    pub async fn directory_sync_state(
        &self,
        enterprise_id: &str,
    ) -> Result<Option<DirectorySyncStateDto>, EnterpriseError> {
        let row = sqlx::query_as::<_, DirectorySyncStateDto>(
            "SELECT provider, last_run_at, last_status, last_error, department_count, people_count \
             FROM one_directory_sync_state WHERE enterprise_id = ?",
        )
        .bind(enterprise_id)
        .fetch_optional(self.pool_ref())
        .await?;
        Ok(row)
    }
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySyncStateDto {
    pub provider: String,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub department_count: i64,
    pub people_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// one-sso owns `one_sso_identities`; stand up just enough of it here so
    /// the join can be exercised without depending on that crate.
    async fn service() -> EnterpriseService {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_enterprise_migrations(db.pool()).await.unwrap();
        sqlx::query(
            "CREATE TABLE one_sso_identities (\
                provider TEXT NOT NULL, external_id TEXT NOT NULL, user_id TEXT NOT NULL)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        EnterpriseService::new(db.pool().clone())
    }

    fn person(external_id: &str, active: bool) -> DirectoryPersonInput {
        DirectoryPersonInput {
            external_id: external_id.into(),
            name: Some(format!("name-{external_id}")),
            job_title: None,
            department_external_id: Some("od_1".into()),
            active,
        }
    }

    fn snapshot(people: Vec<DirectoryPersonInput>, complete: bool) -> DirectorySyncInput {
        DirectorySyncInput {
            provider: "feishu".into(),
            external_id_field: "open_id".into(),
            departments: vec![DirectoryDepartmentInput {
                external_id: "od_1".into(),
                parent_external_id: None,
                name: "研发中心".into(),
            }],
            people,
            complete,
            error: None,
        }
    }

    /// one-org owns the project-group tables. Stand up just enough of them to
    /// exercise the join; tests that skip this are the standalone deployment,
    /// where the tables genuinely do not exist.
    async fn with_project_groups(svc: &EnterpriseService) {
        sqlx::query("CREATE TABLE one_tenants (id TEXT PRIMARY KEY, name TEXT NOT NULL)")
            .execute(svc.pool_ref())
            .await
            .unwrap();
        sqlx::query("CREATE TABLE one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, role TEXT NOT NULL)")
            .execute(svc.pool_ref())
            .await
            .unwrap();
    }

    async fn join_group(svc: &EnterpriseService, user_id: &str, tenant_id: &str, name: &str) {
        sqlx::query("INSERT OR IGNORE INTO one_tenants (id, name) VALUES (?, ?)")
            .bind(tenant_id)
            .bind(name)
            .execute(svc.pool_ref())
            .await
            .unwrap();
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role) VALUES (?, ?, 'member')")
            .bind(user_id)
            .bind(tenant_id)
            .execute(svc.pool_ref())
            .await
            .unwrap();
    }

    async fn make_departed(svc: &EnterpriseService, external_id: &str, user_id: &str) {
        svc.apply_directory_snapshot("ent1", &snapshot(vec![person(external_id, true)], true))
            .await
            .unwrap();
        svc.apply_directory_snapshot("ent1", &snapshot(vec![], true))
            .await
            .unwrap();
        bind_identity(svc, external_id, user_id).await;
        sqlx::query(
            "INSERT INTO one_enterprise_members (user_id, enterprise_id, display_name, role, joined_at, updated_at) \
             VALUES (?, 'ent1', ?, 'member', 0, 0)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(svc.pool_ref())
        .await
        .unwrap();
    }

    async fn bind_identity(svc: &EnterpriseService, external_id: &str, user_id: &str) {
        sqlx::query("INSERT INTO one_sso_identities (provider, external_id, user_id) VALUES ('feishu', ?, ?)")
            .bind(external_id)
            .bind(user_id)
            .execute(svc.pool_ref())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_complete_pull_flags_whoever_stopped_appearing() {
        let svc = service().await;
        svc.apply_directory_snapshot(
            "ent1",
            &snapshot(vec![person("ou_a", true), person("ou_b", true)], true),
        )
        .await
        .unwrap();

        // ou_b is gone from the company.
        let report = svc
            .apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", true)], true))
            .await
            .unwrap();

        assert_eq!(report.newly_missing, 1);
        assert!(report.complete);
    }

    /// ⚠️ The one that matters. A pull that did not finish looks exactly like a
    /// company where everybody quit; acting on it would remove real people's
    /// access and reassign their work.
    #[tokio::test]
    async fn an_incomplete_pull_never_flags_anyone() {
        let svc = service().await;
        svc.apply_directory_snapshot(
            "ent1",
            &snapshot(vec![person("ou_a", true), person("ou_b", true)], true),
        )
        .await
        .unwrap();

        // Same "everyone but ou_a is missing" shape as above — but incomplete.
        let report = svc
            .apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", true)], false))
            .await
            .unwrap();

        assert_eq!(report.newly_missing, 0, "an incomplete pull must not flag departures");
        assert!(!report.complete);

        bind_identity(&svc, "ou_b", "u_b").await;
        svc.sync_member("u_b", "feishu", "ent-ext", None, None, None)
            .await
            .unwrap();
        assert!(
            svc.list_departed_members("ent1").await.unwrap().is_empty(),
            "nobody may be proposed for offboarding off a partial pull"
        );
    }

    /// Feishu keeps leavers listed and flags them, so "still in the directory"
    /// is not "still employed".
    #[tokio::test]
    async fn a_resigned_person_is_flagged_even_though_still_listed() {
        let svc = service().await;
        svc.apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", true)], true))
            .await
            .unwrap();
        let report = svc
            .apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", false)], true))
            .await
            .unwrap();
        assert_eq!(report.newly_missing, 1);
    }

    /// A rehire must lose the flag, or the console keeps proposing to offboard
    /// somebody who is back at work.
    #[tokio::test]
    async fn someone_who_comes_back_is_unflagged() {
        let svc = service().await;
        svc.apply_directory_snapshot(
            "ent1",
            &snapshot(vec![person("ou_a", true), person("ou_b", true)], true),
        )
        .await
        .unwrap();
        svc.apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", true)], true))
            .await
            .unwrap();

        let report = svc
            .apply_directory_snapshot(
                "ent1",
                &snapshot(vec![person("ou_a", true), person("ou_b", true)], true),
            )
            .await
            .unwrap();
        assert_eq!(report.returned, 1);
        assert_eq!(report.newly_missing, 0);
    }

    /// Only people with a bound local account can be offboarded — a directory
    /// row on its own is not a member of anything here.
    #[tokio::test]
    async fn only_bound_members_are_proposed_for_offboarding() {
        let svc = service().await;
        svc.apply_directory_snapshot(
            "ent1",
            &snapshot(vec![person("ou_a", true), person("ou_b", true)], true),
        )
        .await
        .unwrap();
        svc.apply_directory_snapshot("ent1", &snapshot(vec![person("ou_a", true)], true))
            .await
            .unwrap();

        // ou_b is gone but never had an account here.
        assert!(
            svc.list_departed_members("ent1").await.unwrap().is_empty(),
            "a directory row alone is not a member of anything here"
        );

        // Now give them one. Inserted directly rather than via `sync_member`,
        // which resolves the company itself — this test is about the join, not
        // about company resolution.
        bind_identity(&svc, "ou_b", "u_b").await;
        sqlx::query(
            "INSERT INTO one_enterprise_members (user_id, enterprise_id, display_name, role, joined_at, updated_at) \
             VALUES ('u_b', 'ent1', '李四', 'member', 0, 0)",
        )
        .execute(svc.pool_ref())
        .await
        .unwrap();

        let departed = svc.list_departed_members("ent1").await.unwrap();
        assert_eq!(departed.len(), 1);
        assert_eq!(departed[0].user_id, "u_b");
        assert_eq!(departed[0].display_name.as_deref(), Some("李四"));
    }

    /// Removing somebody is project-group scoped, so the console has to be told
    /// which groups they are in before it can offer to do it.
    #[tokio::test]
    async fn a_departure_carries_the_project_groups_the_person_is_still_in() {
        let svc = service().await;
        with_project_groups(&svc).await;
        make_departed(&svc, "ou_b", "u_b").await;
        join_group(&svc, "u_b", "t_1", "研发组").await;
        join_group(&svc, "u_b", "t_2", "市场组").await;
        // Somebody else's membership must not leak onto this row.
        join_group(&svc, "u_other", "t_3", "财务组").await;

        let departed = svc.list_departed_members("ent1").await.unwrap();
        assert_eq!(departed.len(), 1);
        let mut names: Vec<&str> = departed[0].tenants.iter().filter_map(|t| t.name.as_deref()).collect();
        names.sort_unstable();
        assert_eq!(names, ["市场组", "研发组"]);
        assert!(
            departed[0].tenants.iter().all(|t| t.tenant_id != "t_3"),
            "another user's project group must not leak onto this row"
        );
    }

    /// A company member who never joined a project group is still a departure —
    /// they just only need the company-level removal. An empty list here is an
    /// answer, not a missing one.
    #[tokio::test]
    async fn a_member_in_no_project_group_reports_an_empty_group_list() {
        let svc = service().await;
        with_project_groups(&svc).await;
        make_departed(&svc, "ou_b", "u_b").await;

        let departed = svc.list_departed_members("ent1").await.unwrap();
        assert_eq!(departed.len(), 1);
        assert!(departed[0].tenants.is_empty());
    }

    /// Standalone/personal deployments have no project-group tables at all.
    /// The departed list must still render rather than 500.
    #[tokio::test]
    async fn a_deployment_without_project_group_tables_still_lists_departures() {
        let svc = service().await;
        make_departed(&svc, "ou_b", "u_b").await;

        let departed = svc.list_departed_members("ent1").await.unwrap();
        assert_eq!(departed.len(), 1);
        assert!(departed[0].tenants.is_empty());
    }

    /// The status line has to say a sync failed. A silent failure reads as
    /// "the directory really is empty".
    #[tokio::test]
    async fn a_failed_pull_is_recorded_as_partial_with_its_reason() {
        let svc = service().await;
        let mut input = snapshot(vec![], false);
        input.error = Some("tenant token: HTTP 500".into());
        svc.apply_directory_snapshot("ent1", &input).await.unwrap();

        let state = svc.directory_sync_state("ent1").await.unwrap().unwrap();
        assert_eq!(state.last_status.as_deref(), Some("partial"));
        assert_eq!(state.last_error.as_deref(), Some("tenant token: HTTP 500"));
    }

    /// Red line: the mirror must never touch the seat table.
    #[tokio::test]
    async fn syncing_a_directory_creates_no_members_and_therefore_no_seats() {
        let svc = service().await;
        let people: Vec<DirectoryPersonInput> = (0..50).map(|i| person(&format!("ou_{i}"), true)).collect();
        svc.apply_directory_snapshot("ent1", &snapshot(people, true))
            .await
            .unwrap();

        let seats: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM one_enterprise_members")
            .fetch_one(svc.pool_ref())
            .await
            .unwrap();
        assert_eq!(seats, 0, "a directory pull must not consume licensed seats");
    }
}
