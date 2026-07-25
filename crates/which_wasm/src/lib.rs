#![cfg_attr(target_family = "wasm", allow(unused))]

#[cfg(not(target_family = "wasm"))]
pub use which_real::*;

#[cfg(target_family = "wasm")]
pub use stub::*;

#[cfg(target_family = "wasm")]
mod stub {
    use std::ffi::OsStr;
    use std::fmt;
    use std::path::{Path, PathBuf};

    pub type Result<T> = std::result::Result<T, Error>;

    #[derive(Copy, Clone, Eq, PartialEq, Debug)]
    pub enum Error {
        CannotFindBinaryPath,
        CannotGetCurrentDirAndPathListEmpty,
        CannotCanonicalize,
    }

    impl std::error::Error for Error {}

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Error::CannotFindBinaryPath => write!(f, "cannot find binary path"),
                Error::CannotGetCurrentDirAndPathListEmpty => write!(
                    f,
                    "no path to search and provided name is not an absolute path"
                ),
                Error::CannotCanonicalize => write!(f, "cannot canonicalize path"),
            }
        }
    }

    pub fn which<T: AsRef<OsStr>>(_binary_name: T) -> Result<PathBuf> {
        Err(Error::CannotFindBinaryPath)
    }

    pub fn which_global<T: AsRef<OsStr>>(_binary_name: T) -> Result<PathBuf> {
        Err(Error::CannotFindBinaryPath)
    }

    pub fn which_all<T: AsRef<OsStr>>(_binary_name: T) -> Result<impl Iterator<Item = PathBuf>> {
        Err::<std::iter::Empty<PathBuf>, _>(Error::CannotFindBinaryPath)
    }

    pub fn which_all_global<T: AsRef<OsStr>>(
        _binary_name: T,
    ) -> Result<impl Iterator<Item = PathBuf>> {
        Err::<std::iter::Empty<PathBuf>, _>(Error::CannotFindBinaryPath)
    }

    pub fn which_in<T, U, V>(_binary_name: T, _paths: Option<U>, _cwd: V) -> Result<PathBuf>
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>,
        V: AsRef<Path>,
    {
        Err(Error::CannotFindBinaryPath)
    }

    pub fn which_in_all<T, U, V>(
        _binary_name: T,
        _paths: Option<U>,
        _cwd: V,
    ) -> Result<impl Iterator<Item = PathBuf>>
    where
        T: AsRef<OsStr>,
        U: AsRef<OsStr>,
        V: AsRef<Path>,
    {
        Err::<std::iter::Empty<PathBuf>, _>(Error::CannotFindBinaryPath)
    }
}
