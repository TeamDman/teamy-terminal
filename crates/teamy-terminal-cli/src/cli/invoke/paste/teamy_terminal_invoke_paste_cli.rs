use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> paste`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct PasteArgs {
    #[facet(args::positional)]
    pub text: String,
}
