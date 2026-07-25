use arbitrary::Arbitrary;
use facet::Facet;

/// Arguments for `teamy-terminal invoke <id> close`.
#[derive(Arbitrary, Debug, Facet, Default, PartialEq, Eq)]
pub struct CloseArgs {}
