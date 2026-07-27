use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> zoom`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct ZoomArgs {
    /// Signed one-pixel font-size steps; positive grows, negative shrinks.
    #[facet(args::positional)]
    pub steps: i16,
}
