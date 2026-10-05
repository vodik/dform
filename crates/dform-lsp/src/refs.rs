//! Definition and references: what a name at a place denotes and every
//! place that denotes the same, as `dform_core::names` reads them off the
//! project's files (R-78); on an attribute path, the contributions to its
//! cell the evaluation found (the hover's list).

use crate::analysis::{self, Evaluated, Where};
use crate::{explain, text};
use dform_core::circuit::View;
use dform_core::names::{Decls, Parsed, Site, What};
use lsp_types::Location;
use std::path::{Path, PathBuf};

/// A project as references read it: its files' texts (open buffers
/// included) and its stacks' evaluations in the selected environment.
pub struct Project<'a> {
    /// Where the evaluations ran: their places are relative to it.
    pub dir: PathBuf,
    pub files: Vec<(PathBuf, String)>,
    pub evaluated: Vec<&'a Evaluated>,
    /// The providers' schema files, each read: what a type's definition
    /// is found in, and what picks among resources of one name with no
    /// evaluation to read.
    pub schemas: Vec<(PathBuf, std::sync::Arc<dform_core::schema::Schema>)>,
}

impl Project<'_> {
    pub fn parse(&self) -> Vec<Parsed> {
        self.files
            .iter()
            .map(|(path, text)| Parsed::new(path.clone(), text.clone()))
            .collect()
    }

    /// The declarations of the parsed files, with the evaluated schemas'
    /// types (a `ref(T)` picks among resources of one name, R-74).
    pub fn decls(&self, files: &[Parsed]) -> Decls {
        let mut d = Decls::of_files(&self.dir, files);
        for e in &self.evaluated {
            d = d.with_schema(&e.schema);
        }
        for (_, s) in &self.schemas {
            d = d.with_schema(s);
        }
        d
    }
}

/// `textDocument/references` at byte `at` of `path`: every place that
/// denotes what the name there does, through `use` and instance scopes;
/// a resource's also where a string is its address (H-16).
pub fn references(p: &Project, path: &Path, at: usize, declarations: bool) -> Vec<Location> {
    let files = p.parse();
    let d = p.decls(&files);
    let Some(here) = d.at(&files, path, at) else {
        return Vec::new();
    };
    let syms = match here.what {
        What::Name(sym, _) => vec![sym],
        What::Names(syms) => syms,
        What::Path => return contributors(p, path, at),
        _ => return Vec::new(),
    };
    let mut out: Vec<Location> = Vec::new();
    let mut push = |f: &Parsed, r: rowan::TextRange| {
        let l = Location::new(
            text::uri_of(&f.path),
            text::range(&f.text, r.start().into(), r.end().into()),
        );
        if !out.contains(&l) {
            out.push(l);
        }
    };
    for sym in &syms {
        for n in d.occurrences(&files, sym) {
            if declarations || !n.is_declaration() {
                push(n.file, n.range);
            }
        }
        let addresses = d.addresses_of(sym);
        for (f, t, typ, a) in dform_core::names::addresses(&files) {
            if addresses.contains(&(typ, a)) {
                push(f, t.text_range());
            }
        }
    }
    out
}

/// `textDocument/definition` at byte `at` of `path`: where the name there
/// is declared. A function's is its signature line in the std file
/// `std_file` extracts; a type's, its `type` blocks and the line of the
/// schema file that declares it.
pub fn definition(
    p: &Project,
    path: &Path,
    at: usize,
    std_file: &dyn Fn(&str, &str) -> Option<PathBuf>,
    schema_files: &[PathBuf],
) -> Vec<Location> {
    let files = p.parse();
    let d = p.decls(&files);
    let Some(what) = d.at(&files, path, at).map(|n| n.what) else {
        return Vec::new();
    };
    let mut out: Vec<Location> = d
        .definition(&files, &what)
        .into_iter()
        .filter_map(|s| match s {
            Site::Text(f, r) => Some(Location::new(
                text::uri_of(&f.path),
                text::range(&f.text, r.start().into(), r.end().into()),
            )),
            Site::File(f) => Some(Location::new(text::uri_of(&f), text::range("", 0, 0))),
            Site::Std(file, source, line) => {
                let f = std_file(file, source)?;
                // From the name after `fn` or `package`.
                let l = source.lines().nth(line.saturating_sub(1)).unwrap_or("");
                let col = l.find(' ').map_or(1, |i| i + 2);
                Some(Location::new(
                    text::uri_of(&f),
                    text::line_range(source, line, col),
                ))
            }
        })
        .collect();
    // A type's schema declaration beside the program's `type` blocks.
    if let What::Type(typ) = &what {
        out.extend(schema_type(schema_files, typ));
    }
    out
}

/// The first fact of a schema file (a provider's `schema.df`, R-24) about
/// the type `typ`: its first argument, quoted or not.
fn schema_type(files: &[PathBuf], typ: &str) -> Option<Location> {
    files.iter().find_map(|f| {
        let t = std::fs::read_to_string(f).ok()?;
        let at = [format!("(\"{typ}\""), format!("({typ},")]
            .iter()
            .filter_map(|w| t.find(w.as_str()))
            .min()?;
        let start = at + 1 + usize::from(t[at + 1..].starts_with('"'));
        Some(Location::new(
            text::uri_of(f),
            text::range(&t, start, start + typ.len()),
        ))
    })
}

/// Where each contribution to the attributes the code at `at` is about is
/// written: every rule, module instance's or pack's, contributing to
/// the cell, as the hover lists them.
pub fn contributors(p: &Project, path: &Path, at: usize) -> Vec<Location> {
    let mut out: Vec<Location> = Vec::new();
    for e in &p.evaluated {
        let in_file = |id: u32| e.files.get(&id).is_some_and(|f| f == path);
        let c = &e.res.circuit;
        for n in explain::targets(e, &in_file, at) {
            let View::Fact { fact, alts, .. } = c.view(n) else {
                continue;
            };
            if fact.pred != "attr" {
                continue;
            }
            for a in alts {
                let View::Times { children, .. } = c.view(*a) else {
                    continue;
                };
                for ch in children {
                    if !matches!(c.view(*ch), View::Fact { fact, .. } if fact.pred == "arg") {
                        continue;
                    }
                    let Some(l) = analysis::written(e, *ch).and_then(|w| locate(p, e, w)) else {
                        continue;
                    };
                    if !out.contains(&l) {
                        out.push(l);
                    }
                }
            }
        }
    }
    out
}

/// A place an evaluation names, as a location.
fn locate(p: &Project, e: &Evaluated, w: Where) -> Option<Location> {
    let text_of = |f: &Path| {
        p.files
            .iter()
            .find(|(g, _)| g == f)
            .map(|(_, t)| t.clone())
            .or_else(|| std::fs::read_to_string(f).ok())
    };
    match w {
        Where::Span(s) => {
            let f = e.files.get(&s.file)?;
            let t = text_of(f)?;
            Some(Location::new(
                text::uri_of(f),
                text::range(&t, s.start as usize, s.end as usize),
            ))
        }
        Where::Place(place) => {
            // `modules/network.df:15:5 (arg)`.
            let at = place.split(' ').next()?;
            let mut it = at.rsplitn(3, ':');
            let (col, line, name) = (it.next()?, it.next()?, it.next()?);
            let f = p.dir.join(name);
            let f = std::fs::canonicalize(&f).unwrap_or(f);
            let t = text_of(&f)?;
            Some(Location::new(
                text::uri_of(&f),
                text::line_range(&t, line.parse().ok()?, col.parse().ok()?),
            ))
        }
        _ => None,
    }
}
