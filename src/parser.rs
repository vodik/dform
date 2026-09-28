//! Entry points: source text to `ast::Program`, or to diagnostics. The
//! grammar is docs/grammar.md; the parser is `syntax::parser`, the lowering
//! to the AST `syntax::lower`.

use crate::ast::{Program, Span};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::syntax::lower::Lowerer;
use crate::syntax::parser::{self as p, Parse};
use anyhow::Result;

fn syntax_errors(file: u32, parse: &Parse) -> Vec<Diagnostic> {
    parse
        .errors
        .iter()
        .map(|e| {
            let d = Diagnostic::error(
                Span {
                    file,
                    start: e.start as u32,
                    end: e.end as u32,
                },
                e.message.clone(),
            );
            match &e.hint {
                Some(h) => d.with_help(h),
                None => d,
            }
        })
        .collect()
}

fn program(name: &str, src: &str, require_edition: bool) -> Result<Program> {
    let file = diag::add_source(name, src);
    let parse = p::parse(src);
    let errors = syntax_errors(file, &parse);
    if !errors.is_empty() {
        return Err(Diagnostics(errors).into());
    }
    let mut l = Lowerer::new(file);
    let program = l.file(&parse.syntax(), require_edition);
    if !l.diags.is_empty() {
        return Err(Diagnostics(l.diags).into());
    }
    Ok(program)
}

/// A `.df` file: it must start with `edition 2026.`. `name` is how
/// diagnostics name the file.
pub fn parse_file(name: &str, src: &str) -> Result<Program> {
    program(name, src, true)
}

/// Program text that is not a file (tests, provider schemas, snippets): the
/// edition pragma is optional.
pub fn parse_program(src: &str) -> Result<Program> {
    program("<input>", src, false)
}
