use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> send-text`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct SendTextArgs {
    #[facet(args::positional)]
    pub text: String,
}
