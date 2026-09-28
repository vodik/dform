//! The syntax layer: a lossless tree (rowan) the parser builds, the lowering
//! from it to `ast`, and the formatter's view of it.

mod kind;
pub mod parser;
pub mod resolve;

pub use kind::{Lang, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
