#[derive(Debug, thiserror::Error)]
pub enum ClaudeBridgeError {
    #[error("database error: {0}")]
    Db(#[from] aionui_db::DbError),
}
