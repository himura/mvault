//! The error type shared by the commands.

use std::io;
use std::path::PathBuf;

use crate::download::DownloadError;

/// The error returned by operations on a mirror or a workspace.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The blob store failed; the message names the path.
    #[error("{0}")]
    Store(io::Error),
    /// The state database failed.
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    /// A request to an upstream or a mirror failed.
    #[error(transparent)]
    Download(#[from] DownloadError),
    /// An upstream answered with something that cannot be used.
    #[error("{url}: {reason}")]
    Upstream {
        /// The requested URL.
        url: String,
        /// What is wrong with the answer.
        reason: String,
    },
    /// The input or the stored state does not allow the operation.
    #[error("{0}")]
    Invalid(String),
}

/// The result type shared by the commands.
pub type Result<T> = std::result::Result<T, Error>;
