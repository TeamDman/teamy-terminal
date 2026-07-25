use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> resize`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct ResizeArgs {
    #[facet(args::positional)]
    pub width: u32,
    #[facet(args::positional)]
    pub height: u32,
}
