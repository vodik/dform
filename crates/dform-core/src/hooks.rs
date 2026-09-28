//! Planted bugs for the property test (`crates/dform-proptest`), which must
//! catch each one: compiled only with the `test-hooks` feature, which only
//! that crate's dev-dependency enables, and set per thread, so a test that
//! plants one does not reach the tests beside it. Without the feature the
//! evaluator's `planted` is a constant `false`.

use std::cell::Cell;

/// A clause of Rule 3 (E §2.7, F DR-2 revised) the evaluator can be told to
/// skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule3 {
    /// Per key, as specified: nothing skipped.
    PerKey,
    /// `not p(t)` is decided against the current `p` even while a stuck
    /// head of `p` unifies with `p(t)`.
    Negation,
    /// A positive reader of an undetermined aggregate group reads the
    /// groups that were decided and is not itself undetermined (the
    /// mistake F §1.2 item 1 names).
    Reader,
}

thread_local! {
    static RULE3: Cell<Rule3> = const { Cell::new(Rule3::PerKey) };
}

/// Plant `r` on this thread until the guard drops.
pub fn plant(r: Rule3) -> Planted {
    RULE3.with(|c| c.set(r));
    Planted(())
}

/// The planted clause's guard: dropping it restores Rule 3 per key.
pub struct Planted(());

impl Drop for Planted {
    fn drop(&mut self) {
        RULE3.with(|c| c.set(Rule3::PerKey));
    }
}

/// Is `r` planted on this thread?
pub fn planted(r: Rule3) -> bool {
    RULE3.with(|c| c.get()) == r
}
