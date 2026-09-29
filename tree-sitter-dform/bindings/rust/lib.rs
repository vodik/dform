//! The dform grammar for the [tree-sitter] parsing library, for editors:
//! highlighting, indentation and navigation. The compiler's own parser is
//! `dform::syntax::parser`; `tests/treesit_agreement.rs` in the dform
//! repository holds the two to the same corpus.
//!
//! ```
//! let mut parser = tree_sitter::Parser::new();
//! parser
//!     .set_language(&tree_sitter_dform::LANGUAGE.into())
//!     .expect("Error loading dform parser");
//! let tree = parser.parse("edition 2026\np(\"a\")\n", None).unwrap();
//! assert!(!tree.root_node().has_error());
//! ```
//!
//! [tree-sitter]: https://tree-sitter.github.io/

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_dform() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for this grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dform) };

/// The content of the [`node-types.json`] file for this grammar.
///
/// [`node-types.json`]: https://tree-sitter.github.io/tree-sitter/using-parsers/6-static-node-types
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

/// The syntax highlighting query. Field-value dots are captured
/// `@variable.reference` (proposal G, G-6).
pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");

/// The indentation query (nvim-treesitter's captures).
pub const INDENTS_QUERY: &str = include_str!("../../queries/indents.scm");

/// The local variable query.
pub const LOCALS_QUERY: &str = include_str!("../../queries/locals.scm");
