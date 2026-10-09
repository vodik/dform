//! The derivation printer (R-63): `dform why`, and under each deformation
//! of `plan --why` and `diff --since`: a fact's derivation tree, read from
//! the provenance circuit (E §3.3). Each fact prints with the firing that
//! derived it (rule id and text, the rule's bindings) and that firing's
//! children, recursively; a given fact prints with where it came from. An
//! aggregate prints every contribution with its rank and owner. A fact
//! with several alternatives shows the first and `...` for the rest unless
//! `all`; a fact already expanded above prints `(see above)`.
//!
//! An `attr` or `arg` pattern may name part of an object attribute, by a
//! dotted path (`"tags.team"`) or an object value (`{team: "platform"}`):
//! it matches the attribute that contains it, and the tree shows only the
//! contributions that do.
//!
//! `printer` prints the tree in the core's spelling, `surface` in the
//! program's own terms (its statements by `statement`); `because` and
//! `compress` say a deformation's derivation by its leaves; `sites` says
//! where a fact is derived, `chains` how a value was made, `docrow` the
//! row of a document a value was read from.

mod because;
mod chains;
mod compress;
mod docrow;
mod printer;
mod sites;
mod statement;
mod surface;
pub use because::Because;
pub use chains::Step;
pub use docrow::DocRow;
pub use printer::{Focus, Printer, find};
pub use sites::{Site, cell_name};
