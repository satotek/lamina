use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// User-facing text when a cancel request or a closed session kills the git child.
pub const OPERATION_CANCELLED: &str = "the operation was cancelled";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Any error coming from gix. Boxed so modules don't need their own variants.
    #[error("git read error: {0}")]
    Gix(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    #[error("`git {args}` failed (exit {code:?}): {stderr}")]
    GitCommand {
        args: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn gix(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Gix(Box::new(err))
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
