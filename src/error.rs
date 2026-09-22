use crate::backend::BackendError;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Backend(BackendError),
    Tar(crate::tar::TarError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Backend(e) => write!(f, "decompression error: {e}"),
            Error::Tar(e) => write!(f, "tar error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Backend(e) => Some(e.as_ref()),
            Error::Tar(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<BackendError> for Error {
    fn from(e: BackendError) -> Self {
        Error::Backend(e)
    }
}

impl From<crate::tar::TarError> for Error {
    fn from(e: crate::tar::TarError) -> Self {
        Error::Tar(e)
    }
}
