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

/// The syntax errors of a proposal G parse, as diagnostics naming `name`.
pub fn syntax_diagnostics_g(
    name: &str,
    src: &str,
    parse: &crate::syntax::parse::Parse,
) -> Diagnostics {
    let file = diag::add_source(name, src);
    Diagnostics(
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
            .collect(),
    )
}

/// The proposal G surface (docs/grammar.md, being rewritten): a file of the
/// new grammar, lowered on its own. Replaces `parse_file` when every
/// program is rewritten.
pub fn parse_file_g(name: &str, src: &str, require_edition: bool) -> Result<Program> {
    let file = diag::add_source(name, src);
    let parse = crate::syntax::parse::parse(src);
    let errors: Vec<Diagnostic> = parse
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
        .collect();
    if !errors.is_empty() {
        return Err(Diagnostics(errors).into());
    }
    let units = [crate::syntax::resolve::Unit {
        file,
        root: parse.syntax(),
        imports: None,
    }];
    crate::syntax::resolve::lower(&units, &[0], require_edition, false)
        .map_err(|d| Diagnostics(d).into())
}
