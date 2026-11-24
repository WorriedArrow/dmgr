use std::io;
use std::path::PathBuf;

/// Errors produced by `dmgr`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error not tied to a particular path.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// An I/O error while accessing a specific path.
    #[error("{}: {source}", path.display())]
    PathIo {
        /// The path being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The input image or data structure is malformed.
    #[error("corrupt image: {0}")]
    Corrupt(String),

    /// The input uses a feature this crate does not implement.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The caller supplied an invalid argument or configuration.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A property list could not be parsed or written.
    #[error("property list: {0}")]
    Plist(#[from] plist::Error),
}

/// Convenience alias for `Result<T, dmgr::Error>`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidInput(msg.into())
}

pub(crate) trait PathContext<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> PathContext<T> for io::Result<T> {
    fn with_path(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::PathIo {
            path: path.into(),
            source,
        })
    }
}
