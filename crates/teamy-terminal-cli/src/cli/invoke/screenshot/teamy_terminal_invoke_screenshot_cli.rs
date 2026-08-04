use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};
use std::path::PathBuf;

/// Arguments for `teamy-terminal invoke <id> screenshot`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct ScreenshotArgs {
    /// Relative paths are resolved against the invoking CLI's current directory.
    #[facet(args::positional)]
    pub path: String,
}

impl ScreenshotArgs {
    /// Resolves the requested screenshot path against the current directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the process current directory cannot be read.
    pub fn resolve_path(&self) -> eyre::Result<PathBuf> {
        let path = PathBuf::from(&self.path);
        if path.is_absolute() {
            Ok(path)
        } else {
            Ok(std::env::current_dir()?.join(path))
        }
    }
}
