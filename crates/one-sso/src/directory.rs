//! Bulk directory pull (T6) — the company's org chart, not one login's profile.
//!
//! # Why this exists separately from the login path
//!
//! Until now the only org data this product had arrived one person at a time,
//! at the moment they logged in (`service.rs`'s profile refresh). That is
//! enough to label a member, and useless for the two things a 200-person
//! customer actually asks for: *give me the org chart without anyone typing it
//! in*, and *tell me when someone leaves*. Neither can be answered by data that
//! only appears when the subject shows up — a person who has left is precisely
//! the person who stops logging in.
//!
//! So this pulls the whole directory on a schedule and hands it over as a
//! snapshot.
//!
//! # The one invariant that matters
//!
//! [`DirectorySnapshot::complete`] is the difference between a useful feature
//! and a dangerous one. Downstream, "absent from the directory" is what marks
//! somebody as having left, and acting on that removes their access, rotates
//! their tokens and reassigns their resources. A half-finished pull — one page
//! that 500'd, a token that expired mid-walk — looks exactly like a company
//! where everyone quit at once.
//!
//! Every failure path here therefore returns an *incomplete* snapshot rather
//! than a shorter list, and the storage side refuses to draw departure
//! conclusions from one. This mirrors the `authoritative` flag that the team
//! skill / MCP / model-channel syncs already use for the same reason.
//!
//! # Provider coverage
//!
//! Feishu only, deliberately. It is the one provider with an app-level token,
//! Contact API plumbing, and a `base_url` override that lets a test point it at
//! a mock server. DingTalk and WeCom have absolute hardcoded URLs and no
//! override, so nothing about them could be verified — [`DirectorySource`] is
//! shaped to take them later without changing callers.

use crate::providers::feishu::{DirectoryDepartment, DirectoryPerson, FeishuProvider, FeishuProviderConfig};

/// Which IdP a snapshot came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySource {
    Feishu,
}

impl DirectorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
        }
    }
}

/// The company directory as of one pull.
#[derive(Debug, Clone)]
pub struct DirectorySnapshot {
    pub source: DirectorySource,
    pub departments: Vec<DirectoryDepartment>,
    pub people: Vec<DirectoryPerson>,
    /// Did every request in this run succeed?
    ///
    /// ⚠️ Read the module docs before using this. `false` means the snapshot is
    /// a floor, not a set: everything in it is real, but things not in it may
    /// simply not have been fetched. Nothing may conclude "gone" from it.
    pub complete: bool,
    /// Why it is incomplete, for the admin-facing status line. `None` when
    /// `complete`.
    pub error: Option<String>,
}

impl DirectorySnapshot {
    fn incomplete(source: DirectorySource, error: impl Into<String>) -> Self {
        Self {
            source,
            departments: Vec::new(),
            people: Vec::new(),
            complete: false,
            error: Some(error.into()),
        }
    }
}

/// Pull the whole Feishu directory.
///
/// Never returns `Err`: a failed sync is a *status* the admin console shows,
/// not an error that unwinds a background loop. The distinction callers care
/// about is `complete`, not `Result`.
pub async fn fetch_feishu_directory(config: &FeishuProviderConfig, external_id_field: &str) -> DirectorySnapshot {
    let source = DirectorySource::Feishu;

    // One token for the whole run. The single-user path mints one per call,
    // which is fine for one login and wrong for hundreds of paged requests.
    let token = match FeishuProvider::tenant_access_token(config).await {
        Ok(token) => token,
        Err(e) => return DirectorySnapshot::incomplete(source, format!("tenant token: {e}")),
    };

    let departments = match FeishuProvider::fetch_all_departments(config, &token).await {
        Ok(departments) => departments,
        Err(e) => return DirectorySnapshot::incomplete(source, format!("departments: {e}")),
    };

    // Feishu lists people per department, so the person list is assembled from
    // one call per department. Someone in two departments comes back twice;
    // dedupe on external id, merging the department ids so the extra
    // memberships are not lost.
    let id_type = if external_id_field == "open_id" {
        "open_id"
    } else {
        "union_id"
    };
    let mut people: Vec<DirectoryPerson> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for department in &departments {
        let batch =
            match FeishuProvider::fetch_department_members(config, &token, &department.external_id, id_type).await {
                Ok(batch) => batch,
                // One department failing makes the whole run incomplete. The
                // alternative — skip it and carry on — would report everyone in
                // that department as having left.
                Err(e) => {
                    return DirectorySnapshot::incomplete(
                        source,
                        format!("members of department {}: {e}", department.external_id),
                    );
                }
            };

        for person in batch {
            match seen.get(&person.external_id) {
                Some(&idx) => {
                    let existing: &mut DirectoryPerson = &mut people[idx];
                    for did in person.department_external_ids {
                        if !existing.department_external_ids.contains(&did) {
                            existing.department_external_ids.push(did);
                        }
                    }
                    // A person listed as resigned anywhere is resigned.
                    existing.active &= person.active;
                }
                None => {
                    seen.insert(person.external_id.clone(), people.len());
                    people.push(person);
                }
            }
        }
    }

    DirectorySnapshot {
        source,
        departments,
        people,
        complete: true,
        error: None,
    }
}

/// How often the scheduled sync runs. A directory changes on the timescale of
/// HR paperwork, so this is deliberately slow: the cost of being an hour late
/// noticing a departure is small, and the cost of hammering an IdP's rate limit
/// from every deployment is not.
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 60 * 60;

/// How long to wait before the first run after boot. Startup is already the
/// busiest moment; the directory can wait.
pub const FIRST_RUN_DELAY_SECS: u64 = 60;

/// Drive [`run_directory_sync`] on a timer until shutdown.
///
/// ⚠️ This is spawned by `cmd_server`, which is the same binary every desktop
/// install runs — not a server-only process. It must therefore be harmless on a
/// member's laptop, and it is: the gate inside `run_directory_sync` is "does
/// *this* database hold an enabled Feishu provider and a company". In client
/// mode SSO config lives on the company server, so a member's local database
/// has neither and every tick is two cheap queries that find nothing. Personal
/// installs likewise.
///
/// Errors never escape: a failing sync is a status the console shows, and a
/// background loop that exits on the first network blip would silently stop
/// noticing departures forever.
pub fn start_directory_sync_scheduler(
    sso: std::sync::Arc<crate::service::SsoService>,
    sink: std::sync::Arc<dyn crate::enterprise::DirectorySink>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    interval_secs: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    let interval = std::time::Duration::from_secs(interval_secs.unwrap_or(DEFAULT_SYNC_INTERVAL_SECS));
    tokio::spawn(async move {
        let first = tokio::time::sleep(std::time::Duration::from_secs(FIRST_RUN_DELAY_SECS));
        tokio::pin!(first);
        tokio::select! {
            _ = &mut first => {}
            _ = shutdown.changed() => return,
        }

        loop {
            match run_directory_sync(&sso, sink.as_ref()).await {
                DirectorySyncRun::Skipped(_) => {
                    // Silent on purpose. This is the normal state of every
                    // machine that is not the company server, and logging it
                    // hourly on every desktop would be noise, not diagnosis.
                }
                DirectorySyncRun::Ran(snapshot) if snapshot.complete => {
                    tracing::info!(
                        departments = snapshot.departments.len(),
                        people = snapshot.people.len(),
                        "scheduled directory sync completed"
                    );
                }
                DirectorySyncRun::Ran(snapshot) => {
                    // Loud: an incomplete pull means the mirror is stale AND no
                    // departures were derived from it. Silence here reads as
                    // "nobody has left", which is the wrong conclusion.
                    tracing::warn!(
                        error = snapshot.error.as_deref().unwrap_or("unknown"),
                        "scheduled directory sync did not complete; no departures were derived"
                    );
                }
            }

            let tick = tokio::time::sleep(interval);
            tokio::pin!(tick);
            tokio::select! {
                _ = &mut tick => {}
                _ = shutdown.changed() => return,
            }
        }
    })
}

/// Why a sync run did nothing. Not errors — these are the normal states of a
/// deployment that simply has no directory to sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorySyncSkipped {
    /// No Feishu provider row, or it is disabled. This is also the gate that
    /// keeps the scheduler idle on member laptops and personal installs: SSO
    /// config lives on the company server, so a client's local database has no
    /// such row and the loop finds nothing to do.
    ProviderNotConfigured,
    /// No company has been set up, so there is nothing to attribute a
    /// directory to.
    NoCompany,
}

/// What one run of [`run_directory_sync`] did.
#[derive(Debug, Clone)]
pub enum DirectorySyncRun {
    /// Nothing to do — see [`DirectorySyncSkipped`]. The normal outcome
    /// everywhere except the company server.
    Skipped(DirectorySyncSkipped),
    /// A pull was attempted. Check `complete` on the snapshot: "ran" is not
    /// "succeeded".
    Ran(DirectorySnapshot),
}

/// Run one directory sync, if this deployment is one that should.
///
/// Skipping is deliberately not an error: a personal install idling is correct
/// behaviour, not a fault worth logging.
pub async fn run_directory_sync(
    sso: &crate::service::SsoService,
    sink: &dyn crate::enterprise::DirectorySink,
) -> DirectorySyncRun {
    use DirectorySyncSkipped::*;

    let row = match sso.get_provider_row(crate::models::SsoProviderKind::Feishu).await {
        Ok(Some(row)) if row.enabled => row,
        _ => return DirectorySyncRun::Skipped(ProviderNotConfigured),
    };
    let Some(mut config) = crate::service::parse_feishu_config(&row) else {
        return DirectorySyncRun::Skipped(ProviderNotConfigured);
    };
    // A secret is required to mint the app-level token; without one there is
    // nothing to try.
    if config.app_secret.trim().is_empty() || config.app_secret == "******" {
        return DirectorySyncRun::Skipped(ProviderNotConfigured);
    }

    // Development-only host override, so a run can be pointed at a stand-in
    // Feishu. Deliberately an env var rather than a stored config field:
    // `parse_feishu_config` keeps `base_url` out of admin-editable config on
    // purpose, and turning it into a saved setting would make "where do our
    // company credentials get sent" something an API call can change.
    if let Ok(base) = std::env::var("AIONUI_FEISHU_BASE_URL")
        && !base.trim().is_empty()
    {
        tracing::warn!(base = %base, "directory sync using AIONUI_FEISHU_BASE_URL override (development only)");
        config.base_url = Some(base);
    }

    let Some(enterprise_id) = sink.enterprise_id().await else {
        return DirectorySyncRun::Skipped(NoCompany);
    };

    let external_id_field = config.external_id_field.clone();
    let snapshot = fetch_feishu_directory(&config, &external_id_field).await;
    sink.apply_snapshot(&enterprise_id, snapshot.clone().into_payload(&external_id_field))
        .await;
    DirectorySyncRun::Ran(snapshot)
}

impl DirectorySnapshot {
    /// Hand this snapshot across the crate seam.
    pub fn into_payload(self, external_id_field: &str) -> crate::enterprise::DirectorySnapshotPayload {
        crate::enterprise::DirectorySnapshotPayload {
            provider: self.source.as_str().to_owned(),
            external_id_field: external_id_field.to_owned(),
            departments: self
                .departments
                .into_iter()
                .map(|d| crate::enterprise::DirectoryDepartmentPayload {
                    external_id: d.external_id,
                    parent_external_id: d.parent_external_id,
                    name: d.name,
                })
                .collect(),
            people: self
                .people
                .into_iter()
                .map(|p| crate::enterprise::DirectoryPersonPayload {
                    external_id: p.external_id,
                    name: p.name,
                    job_title: p.job_title,
                    // Only the primary department is modelled; see the schema.
                    department_external_id: p.department_external_ids.into_iter().next(),
                    active: p.active,
                })
                .collect(),
            complete: self.complete,
            error: self.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::feishu::DirectoryPerson;

    fn person(id: &str, departments: &[&str], active: bool) -> DirectoryPerson {
        DirectoryPerson {
            external_id: id.into(),
            name: Some(format!("name-{id}")),
            job_title: None,
            department_external_ids: departments.iter().map(|d| (*d).to_owned()).collect(),
            active,
        }
    }

    /// An incomplete snapshot must be empty, not short. A caller that saw a
    /// partial list would read the missing people as departures.
    #[test]
    fn an_incomplete_snapshot_carries_no_people() {
        let snap = DirectorySnapshot::incomplete(DirectorySource::Feishu, "boom");
        assert!(!snap.complete);
        assert!(snap.people.is_empty());
        assert!(snap.departments.is_empty());
        assert_eq!(snap.error.as_deref(), Some("boom"));
    }

    /// Someone in two departments is one person with two departments, not two
    /// people — otherwise the headcount is wrong and the second row's
    /// department overwrites the first.
    #[test]
    fn a_person_in_two_departments_is_merged_not_duplicated() {
        let mut people: Vec<DirectoryPerson> = Vec::new();
        let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        for p in [person("u1", &["d1"], true), person("u1", &["d2"], true)] {
            match seen.get(&p.external_id) {
                Some(&idx) => {
                    let existing: &mut DirectoryPerson = &mut people[idx];
                    for did in p.department_external_ids {
                        if !existing.department_external_ids.contains(&did) {
                            existing.department_external_ids.push(did);
                        }
                    }
                    existing.active &= p.active;
                }
                None => {
                    seen.insert(p.external_id.clone(), people.len());
                    people.push(p);
                }
            }
        }

        assert_eq!(people.len(), 1);
        assert_eq!(people[0].department_external_ids, vec!["d1", "d2"]);
    }

    // ── The should-I-run gate ───────────────────────────────────────────────
    //
    // ⚠️ These matter more than they look. `cmd_server` is the binary every
    // desktop install runs, so this scheduler ticks on every member's laptop.
    // If the gate ever stopped holding, every one of those machines would start
    // hammering the company's Feishu app — and each would be writing a
    // directory into its own local database, where nobody would ever look at
    // it.

    /// A sink that records whether it was ever asked to store anything.
    struct RecordingSink {
        enterprise: Option<String>,
        applied: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::enterprise::DirectorySink for RecordingSink {
        async fn enterprise_id(&self) -> Option<String> {
            self.enterprise.clone()
        }
        async fn apply_snapshot(&self, _enterprise_id: &str, _snapshot: crate::enterprise::DirectorySnapshotPayload) {
            self.applied.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn sso_with_provider(config: Option<&str>) -> crate::service::SsoService {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_sso_migrations(db.pool()).await.unwrap();
        if let Some(config) = config {
            sqlx::query(
                "INSERT INTO one_sso_providers (provider, enabled, config, updated_at) VALUES ('feishu', 1, ?, 0)",
            )
            .bind(config)
            .execute(db.pool())
            .await
            .unwrap();
        }
        // Same construction as `service.rs`'s harness — kept identical so this
        // never drifts from how the service is really built.
        let user_repo: std::sync::Arc<dyn aionui_db::IUserRepository> =
            std::sync::Arc::new(aionui_db::SqliteUserRepository::new(db.pool().clone()));
        crate::service::SsoService::new(
            db.pool().clone(),
            user_repo,
            std::sync::Arc::new(aionui_auth::JwtService::new("test-secret".into())),
            std::sync::Arc::new(aionui_auth::CookieConfig {
                secure: false,
                same_site: "Lax",
            }),
        )
    }

    /// A member's laptop / a personal install: no SSO provider row here, so the
    /// loop must find nothing to do and touch no network.
    #[tokio::test]
    async fn a_machine_without_sso_config_never_syncs() {
        let sso = sso_with_provider(None).await;
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = RecordingSink {
            enterprise: Some("ent1".into()),
            applied: applied.clone(),
        };

        let run = run_directory_sync(&sso, &sink).await;
        assert!(matches!(
            run,
            DirectorySyncRun::Skipped(DirectorySyncSkipped::ProviderNotConfigured)
        ));
        assert_eq!(applied.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// A provider configured but with no secret cannot mint an app token, so
    /// there is nothing to attempt.
    #[tokio::test]
    async fn a_provider_without_a_secret_never_syncs() {
        let sso = sso_with_provider(Some(r#"{"appId":"cli_x","appSecret":""}"#)).await;
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = RecordingSink {
            enterprise: Some("ent1".into()),
            applied: applied.clone(),
        };

        assert!(matches!(
            run_directory_sync(&sso, &sink).await,
            DirectorySyncRun::Skipped(DirectorySyncSkipped::ProviderNotConfigured)
        ));
        assert_eq!(applied.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Configured, but no company has been set up — nothing to attribute a
    /// directory to, and again no network call.
    #[tokio::test]
    async fn a_deployment_without_a_company_never_syncs() {
        let sso = sso_with_provider(Some(r#"{"appId":"cli_x","appSecret":"s"}"#)).await;
        let applied = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = RecordingSink {
            enterprise: None,
            applied: applied.clone(),
        };

        assert!(matches!(
            run_directory_sync(&sso, &sink).await,
            DirectorySyncRun::Skipped(DirectorySyncSkipped::NoCompany)
        ));
        assert_eq!(applied.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
