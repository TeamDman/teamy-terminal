use arbitrary::Arbitrary;
use facet::Facet;

/// Arguments for `teamy-terminal invoke <id> get-content`.
#[derive(Arbitrary, Debug, Facet, Default, PartialEq, Eq)]
pub struct GetContentArgs {}
