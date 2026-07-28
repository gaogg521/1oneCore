//! Repository trait for the `assistant_marketplace_personas` table.

use crate::error::DbError;
use crate::models::{MarketplacePersonaRow, UpsertMarketplacePersonaParams};

/// Read-mostly catalog of installable marketplace personas, entirely
/// separate from `assistant_definitions` — browsing this table never
/// touches a user's own assistant list. See
/// `AssistantService::import_personas` for the "install" path that
/// materializes a real owned assistant on demand.
#[async_trait::async_trait]
pub trait IAssistantMarketplaceRepository: Send + Sync {
    /// All catalog entries, ordered by name.
    async fn list(&self) -> Result<Vec<MarketplacePersonaRow>, DbError>;

    /// Look up a single catalog entry by id.
    async fn get(&self, id: &str) -> Result<Option<MarketplacePersonaRow>, DbError>;

    /// Upsert the full catalog in one call. Used at startup to keep this
    /// table in sync with the embedded manifest.
    async fn upsert_many(&self, entries: &[UpsertMarketplacePersonaParams<'_>]) -> Result<(), DbError>;

    /// Delete every row whose id is not in `keep_ids`. `upsert_many` never
    /// deletes, so this is required whenever the embedded manifest shrinks
    /// or swaps ids — otherwise old catalog generations accumulate as
    /// orphaned rows forever. Returns the number of rows deleted.
    async fn delete_missing(&self, keep_ids: &[&str]) -> Result<u64, DbError>;
}
