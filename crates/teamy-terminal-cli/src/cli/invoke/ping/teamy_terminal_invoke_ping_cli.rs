use arbitrary::Arbitrary;
use facet::Facet;

/// Arguments for `teamy-terminal invoke <id> ping`.
#[derive(Arbitrary, Debug, Facet, Default, PartialEq, Eq)]
pub struct PingArgs {}
