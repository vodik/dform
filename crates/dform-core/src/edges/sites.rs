//! The sites: each literal, at its node and path, read at the one type
//! the edges its node reaches agree on, and what is said where they do
//! not.

use super::shape::*;
use super::*;

/// A literal to read at its node's type at `path`.
pub(super) struct Site {
    pub(super) node: usize,
    pub(super) path: Path,
    pub(super) span: Span,
    pub(super) holder: Option<Holder>,
}

/// What a literal is written in, as a message names it.
#[derive(Debug, Clone)]
pub(super) enum Holder {
    /// A let's value: `let limits`.
    Let(String),
    /// Anything else, named: `input cidr of component m`.
    Named(String),
}

/// What became of a site.
#[derive(Debug, Clone)]
pub(super) enum Reading {
    /// No edge types it.
    Untyped,
    /// One type, and the edge that gave it.
    At(Ty, Edge),
    /// Two edges that disagree.
    Disagree(Ty, Edge, Ty, Edge),
}

/// The links by their roots: the wholes each part is of, and the parts
/// each whole has, with the path between.
#[derive(Default)]
struct Links {
    wholes: BTreeMap<usize, Vec<(usize, Path)>>,
    parts: BTreeMap<usize, Vec<(usize, Path)>>,
    /// What each value flows into, by their roots.
    pub(super) flows: BTreeMap<usize, Vec<usize>>,
}

impl Graph<'_> {
    /// A literal at `n`'s `path`: found in the first pass, read in the
    /// second.
    pub(super) fn site(&mut self, n: usize, path: Path, t: &mut Term, at: &At) {
        if !at.sites {
            return;
        }
        let i = self.sites.len();
        let span = ambiguous_span(t).unwrap_or(at.span);
        self.sites.push(Site {
            node: n,
            path,
            span,
            holder: at.holder.clone(),
        });
        let Some(readings) = &self.readings else {
            return;
        };
        let site = &self.sites[i];
        let held = std::mem::replace(t, Term::Wildcard);
        let (read, diag) = match readings[i].clone() {
            Reading::Untyped => match types::ambiguous(&held) {
                Some(text)
                    if matches!(site.holder, Some(Holder::Let(_))) && text.ends_with('m') =>
                {
                    (held.clone(), Some(untyped(text, site)))
                }
                _ => (held, None),
            },
            Reading::At(ty, e) => match types::literal(&ty, held.clone()) {
                Ok(x) => (x, None),
                Err(why) => (held, Some(not_its_type(&why, site, &ty, &e))),
            },
            Reading::Disagree(a, ea, b, eb) => {
                let d = disagree(&held, site, (&a, &ea), (&b, &eb));
                (held, Some(d))
            }
        };
        if let Some(d) = diag {
            // Said once: the reading's error, not the position's too.
            *t = match types::ambiguous(&read) {
                Some(text) => Term::Val(Value::Str(text.to_string())),
                None => read,
            };
            self.diags.push(d);
            return;
        }
        // A relation's argument, read at its column's type in turn (R-34).
        *t = match (self.column_types.get(i).cloned().flatten(), read) {
            (Some(ty), read @ Term::Val(_)) => types::literal(&ty, read.clone()).unwrap_or(read),
            (_, read) => read,
        };
    }

    /// The types the edges `n` reaches give its value at `q`, each with
    /// the first edge that gave it; what a sum's other side gives only
    /// where nothing else gives one (`base + 50m`, but `t + 1d` leaves the
    /// time `t` a time).
    fn types_at(&mut self, n: usize, q: &[Seg], by: &Links, depth: usize) -> Vec<(Ty, Edge)> {
        let mut strong: Vec<(Ty, Edge)> = Vec::new();
        let mut weak: Vec<(Ty, Edge)> = Vec::new();
        let add = |out: &mut Vec<(Ty, Edge)>, ty: Ty, e: Edge| {
            if !out.iter().any(|(t, _)| *t == ty) {
                out.push((ty, e));
            }
        };
        let mut seen = BTreeSet::new();
        let mut todo = vec![(self.find(n), q.to_vec())];
        while let Some((r, q)) = todo.pop() {
            if !seen.insert((r, q.clone())) || q.len() > 32 {
                continue;
            }
            for e in self.edges[r].clone() {
                if !e.role.reads() {
                    continue;
                }
                match e.shape {
                    // A side of a sum is the other's type, a duration
                    // beside a time.
                    Shape::Like(m) if depth < 4 => {
                        for (ty, e) in self.types_at(m, &q, by, depth + 1) {
                            let ty = match ty {
                                Ty::Scalar(t) if t == "time" => Ty::Scalar("duration".into()),
                                ty => ty,
                            };
                            add(&mut weak, ty, e);
                        }
                    }
                    Shape::Like(_) => {}
                    _ => {
                        if let Some(ty) = e.shape.at(self.schema, &q) {
                            add(&mut strong, ty, e.at(&q));
                        }
                    }
                }
            }
            // What a value flows into, it reaches; not what else flows in.
            for into in by.flows.get(&r).into_iter().flatten() {
                todo.push((*into, q.clone()));
            }
            for (whole, path) in by.wholes.get(&r).into_iter().flatten() {
                let mut p = path.clone();
                p.extend(q.iter().cloned());
                todo.push((*whole, p));
            }
            for (part, path) in by.parts.get(&r).into_iter().flatten() {
                if let Some(rest) = past(&q, path) {
                    todo.push((*part, rest.to_vec()));
                }
            }
        }
        if strong.is_empty() { weak } else { strong }
    }

    pub(super) fn readings(&mut self) -> Vec<Reading> {
        let mut by = Links::default();
        for (whole, path, part) in std::mem::take(&mut self.links) {
            let (whole, part) = (self.find(whole), self.find(part));
            by.wholes
                .entry(part)
                .or_default()
                .push((whole, path.clone()));
            by.parts.entry(whole).or_default().push((part, path));
        }
        for (from, into) in std::mem::take(&mut self.flows) {
            let (from, into) = (self.find(from), self.find(into));
            if from != into {
                by.flows.entry(from).or_default().push(into);
            }
        }
        (0..self.sites.len())
            .map(|i| {
                let (n, q) = (self.sites[i].node, self.sites[i].path.clone());
                let mut ts = self.types_at(n, &q, &by, 0).into_iter();
                match (ts.next(), ts.next()) {
                    (None, _) => Reading::Untyped,
                    (Some((t, e)), None) => Reading::At(t, e),
                    (Some((a, ea)), Some((b, eb))) => Reading::Disagree(a, ea, b, eb),
                }
            })
            .collect()
    }
}

/// Where an ambiguous quantity literal was written.
fn ambiguous_span(t: &Term) -> Option<Span> {
    types::ambiguous(t)?;
    let Term::Func { args, .. } = t else {
        return None;
    };
    let n = |i: usize| match args.get(i) {
        Some(Term::Val(Value::Int(x))) => u32::try_from(*x).ok(),
        _ => None,
    };
    Some(Span {
        file: n(1)?,
        start: n(2)?,
        end: n(3)?,
        origin: 0,
    })
}

/// How a message names a site: `let limits` at `cpu`.
fn shown_site(site: &Site) -> String {
    let at = match site.path.is_empty() {
        true => String::new(),
        false => format!(" at `{}`", shown_path(&site.path)),
    };
    match &site.holder {
        Some(Holder::Let(k)) => format!("`let {k}`{at}"),
        Some(Holder::Named(n)) => format!("{n}{at}"),
        None => format!("this value{at}"),
    }
}

/// The literal as written.
fn shown_literal(t: &Term) -> String {
    match types::ambiguous(t) {
        Some(text) => text.to_string(),
        None => crate::spell::term(t),
    }
}

/// A site's literal that is not the type `ty` its edge `e` gives it.
fn not_its_type(why: &str, site: &Site, ty: &Ty, e: &Edge) -> Diagnostic {
    Diagnostic::error(site.span, format!("{} {why}", shown_site(site)))
        .with_label(e.span, format!("{ty} here: {}", e.what))
}

/// A site's literal two edges read as two types.
fn disagree(t: &Term, site: &Site, a: (&Ty, &Edge), b: (&Ty, &Edge)) -> Diagnostic {
    let lit = shown_literal(t);
    let within = match &site.holder {
        Some(_) => format!(" in {}", shown_site(site)),
        None => String::new(),
    };
    Diagnostic::error(
        site.span,
        format!(
            "`{lit}`{within} is {} where it reaches {} and {} where it reaches {}: one value \
             has one type",
            a.0, a.1.what, b.0, b.1.what
        ),
    )
    .with_label(a.1.span, format!("{} here", a.0))
    .with_label(b.1.span, format!("{} here", b.0))
    .with_help("give each use its own value, or write the value once per type")
}

/// An ambiguous quantity in a let that nothing it reaches types.
fn untyped(text: &str, site: &Site) -> Diagnostic {
    let k = match &site.holder {
        Some(Holder::Let(k)) => k.as_str(),
        _ => "",
    };
    let mut ty = "cpu".to_string();
    for s in site.path.iter().rev() {
        ty = match s {
            Seg::Key(f) => format!("{{ {f}: {ty} }}"),
            Seg::Elem => format!("list({ty})"),
            Seg::Any => format!("map({ty})"),
        };
    }
    Diagnostic::error(
        site.span,
        format!(
            "`{text}` in {} is millicores in a cpu position and minutes in a duration position, \
             and nothing the let reaches has a type",
            shown_site(site)
        ),
    )
    .with_help(format!(
        "give the let a type, `let {k}: {ty}`, or use it where a cpu or a duration is wanted"
    ))
}
