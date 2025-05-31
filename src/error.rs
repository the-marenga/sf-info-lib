#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum SFSError {
    #[error("DB Error: {0}")]
    DBError(#[from] sqlx::Error),
    #[error("Internal Server Error: {0}")]
    Internal(&'static str),

    #[error("Invalid player: {0}")]
    InvalidPlayer(Box<str>),
    #[error("Invalid scrapbook")]
    InvalidScrapbook,
    #[error("Invalid server")]
    InvalidServer,
}
