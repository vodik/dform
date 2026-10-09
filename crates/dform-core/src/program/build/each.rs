//! `[_]` in a `set` target (R-162, R-211 step 4): each step binds a
//! fresh variable (`Each_AT`, implicit) by a membership goal before the
//! target is read, `x in T` for a resource of the type before it,
//! `x in r.l` for an element of the resource's list before it, with the
//! reads that name the list hoisted before it. A `[_]` in a read is a
//! path step the read enumerates (a hoisted `member`); `v in PATH[_]` is
//! the membership `Coll::Each`.

use super::Builder;
use super::clause::Written;
use crate::program::node::GoalId;

impl Builder<'_> {
    /// The membership a `set` target's `[_]` binds its variable by.
    pub fn each(&mut self, w: &Written) -> Option<GoalId> {
        self.at(w.span, |b| b.membership(w, false, false))
    }
}
