//! The syntax layer: a lossless tree (rowan) the parser builds, the lowering
//! from it to `ast`, its doc comments, and the formatter's view of it.

pub mod doc;
mod kind;
pub mod parser;
pub mod resolve;

pub use kind::{Lang, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

/// A node's own tokens, trivia (whitespace, comments) skipped.
pub fn tokens(n: &SyntaxNode) -> impl Iterator<Item = SyntaxToken> + '_ {
    n.children_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !t.kind().is_trivia())
}
