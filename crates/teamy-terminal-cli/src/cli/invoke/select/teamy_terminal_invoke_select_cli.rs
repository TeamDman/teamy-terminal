use arbitrary::Arbitrary;
use facet::Facet;
use figue::{self as args};

/// Arguments for `teamy-terminal invoke <id> select`.
#[derive(Arbitrary, Debug, Facet, PartialEq, Eq)]
pub struct SelectArgs {
    /// Zero-based anchor column in the visible terminal grid.
    #[facet(args::positional)]
    pub anchor_column: u16,
    /// Zero-based anchor row in the visible terminal grid.
    #[facet(args::positional)]
    pub anchor_row: u16,
    /// Zero-based focus column in the visible terminal grid.
    #[facet(args::positional)]
    pub focus_column: u16,
    /// Zero-based focus row in the visible terminal grid.
    #[facet(args::positional)]
    pub focus_row: u16,
}
