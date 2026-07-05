//! `aioncore resetpass` — reset a user's password directly in the on-disk
//! database.
//!
//! `POST /api/webui/reset-password` is intentionally local-only, which
//! leaves non-local (server / enterprise) deployments without a supported
//! way to set the first password: the default user is created with an
//! empty hash and every login is rejected. This subcommand is the host
//! operator's escape hatch — it needs filesystem access to the data dir,
//! which is the same trust boundary as the database itself.
//!
//! Writes to stdout (not the rolling aioncore.log) for the same reason as
//! `doctor`: the operator runs it interactively and needs the generated
//! password in their terminal.

use std::process::ExitCode;
use std::sync::Arc;

use aionui_auth::{generate_password, hash_password};
use aionui_db::{IUserRepository, SqliteUserRepository, init_database, maybe_copy_legacy_database};

use crate::cli::{Cli, ResetpassArgs};
use crate::commands::error::{CliBoundaryCode, CliBoundaryError};

const SUBCOMMAND: &str = "resetpass";
const RESET_PASSWORD_LEN: usize = 16;

pub async fn run_resetpass(cli: &Cli, args: &ResetpassArgs) -> Result<ExitCode, CliBoundaryError> {
    let db_path = cli.data_dir.join("aionui-backend.db");
    maybe_copy_legacy_database(&db_path).map_err(|_| database_error())?;
    let database = init_database(&db_path).await.map_err(|_| database_error())?;

    let repo: Arc<dyn IUserRepository> = Arc::new(SqliteUserRepository::new(database.pool().clone()));

    let user = match args.username.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(username) => repo.find_by_username(username).await.map_err(|_| database_error())?,
        None => repo.get_primary_webui_user().await.map_err(|_| database_error())?,
    };
    let Some(user) = user else {
        database.close().await;
        return Err(CliBoundaryError::new(
            CliBoundaryCode::CliResetpassUserNotFound,
            SUBCOMMAND,
            "resetpass target user not found",
        ));
    };

    let new_password = generate_password(RESET_PASSWORD_LEN);
    let password_for_hash = new_password.clone();
    let new_hash = tokio::task::spawn_blocking(move || hash_password(&password_for_hash))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| {
            CliBoundaryError::new(
                CliBoundaryCode::CliResetpassHashFailed,
                SUBCOMMAND,
                "resetpass failed to hash the new password",
            )
        })?;

    repo.update_password(&user.id, &new_hash).await.map_err(|_| database_error())?;
    database.close().await;

    println!("Password reset for user: {}", user.username);
    println!("New password: {new_password}");
    println!("Store it now — it is not shown again.");

    Ok(ExitCode::SUCCESS)
}

fn database_error() -> CliBoundaryError {
    CliBoundaryError::new(
        CliBoundaryCode::CliResetpassDatabaseFailed,
        SUBCOMMAND,
        "resetpass failed to open or update the application database",
    )
}
