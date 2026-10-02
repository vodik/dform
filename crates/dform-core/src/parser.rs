//! Entry points: source text to `ast::Program`, or to diagnostics. The
//! grammar is docs/grammar.md; the parser is `syntax::parser`, name
//! resolution and the lowering to the AST `syntax::resolve`. A program of
//! several files is resolved as a whole (`loader::load_program`); these
//! entry points lower one text on its own.

use crate::ast::{Program, Span};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::syntax::parser::{self as p, Parse};
use crate::syntax::resolve::{self, Mode, Unit};
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
                    origin: 0,
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

/// The syntax errors of a parse of `src`, as diagnostics naming `name`.
pub fn syntax_diagnostics(name: &str, src: &str, parse: &Parse) -> Diagnostics {
    Diagnostics(syntax_errors(diag::add_source(name, src), parse))
}

fn program(name: &str, src: &str, require_edition: bool, mode: Mode) -> Result<Program> {
    let file = diag::add_source(name, src);
    let parse = p::parse(src);
    let errors = syntax_errors(file, &parse);
    if !errors.is_empty() {
        return Err(Diagnostics(errors).into());
    }
    let units = [Unit {
        file,
        root: parse.syntax(),
        path: None,
    }];
    resolve::lower(&units, &[0], require_edition, mode).map_err(|d| Diagnostics(d).into())
}

/// A `.df` file on its own: it must start with `edition 2026`. `name` is
/// how diagnostics name the file.
pub fn parse_file(name: &str, src: &str) -> Result<Program> {
    program(name, src, true, Mode::Program)
}

/// Program text that is not a file (tests, provider schemas, snippets): the
/// edition pragma is optional.
pub fn parse_program(src: &str) -> Result<Program> {
    program("<input>", src, false, Mode::Program)
}

/// A `query` or `why` pattern: read on its own, so a dotted name no
/// declaration explains is a type (`attr(leaky.vault, a, .password, v)`).
pub fn parse_pattern(src: &str) -> Result<Program> {
    program("<input>", src, false, Mode::Pattern)
}

/// Text the compiler wrote itself, as a refinement prints: every name is
/// its own text (`type(int)`, `enum(["a"])`), nothing is a variable, and a
/// string's braces are its own (`regex("^[a-z]{3}$")`).
pub fn parse_literal_text(src: &str) -> Result<Program> {
    program("<input>", src, false, Mode::Text)
}
