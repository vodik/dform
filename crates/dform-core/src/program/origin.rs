//! Where a lowered statement comes from (R-211): the item it is lowered
//! out of, the instance of that item's definition (a copy, a `use`; none
//! for the definition as written), and the call site of a `let` with
//! parameters it is a copy for. `lower` returns one per statement it
//! emits, beside the statements; the messages that spell a rule through
//! its origin read them from step 10.

use super::node::{ExprId, ItemId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Origin {
    pub item: ItemId,
    /// An index into `lower`'s instance tree (step 6); none for the
    /// definition itself.
    pub instance: Option<u32>,
    /// The call a site copy is for (step 7).
    pub site: Option<ExprId>,
}

impl Origin {
    /// The item as written: no instance, no site.
    pub fn of(item: ItemId) -> Origin {
        Origin {
            item,
            instance: None,
            site: None,
        }
    }
}
