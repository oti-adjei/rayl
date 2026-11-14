use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// macOS privacy controls (TCC) blocked the read. The host process needs Full Disk Access;
    /// no amount of retrying from inside this process will change that.
    #[error("access to {path} was denied by macOS privacy controls")]
    AccessDenied { path: PathBuf },

    #[error("no Apple Mail data directory at {path}")]
    MailDirMissing { path: PathBuf },

    /// The Mail directory is readable but holds no `V<n>` store, which is what Mail creates
    /// once an account is configured.
    #[error("no versioned mail store (V<n>) found under {path}")]
    NoVersionedStore { path: PathBuf },

    #[error("no envelope index at {path}")]
    EnvelopeIndexMissing { path: PathBuf },

    #[error("HOME is not set, so the mail directory cannot be located")]
    NoHome,

    #[error("reading {path} failed")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("querying the envelope index failed")]
    Sqlite(#[from] rusqlite::Error),
}

impl Error {
    /// Maps an io error to the privacy-vs-absence distinction the diagnosis depends on.
    pub(crate) fn from_io(path: &std::path::Path, source: io::Error) -> Self {
        match source.kind() {
            io::ErrorKind::PermissionDenied => Error::AccessDenied { path: path.into() },
            io::ErrorKind::NotFound => Error::MailDirMissing { path: path.into() },
            _ => Error::Io { path: path.into(), source },
        }
    }

    pub fn is_access_denied(&self) -> bool {
        matches!(self, Error::AccessDenied { .. })
    }
}
