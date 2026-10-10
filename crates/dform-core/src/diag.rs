//! Diagnostics: a message at a span, with labels, notes and a hint, printed
//! in the plan's grammar ([`render`]). Sources are registered once per load so a `Span` (a
//! source id and a byte range) can name `file:line:col` anywhere later.
//!
//! The registry lives as long as what refers to it: a [`Scope`] (one
//! evaluation of a long-running controller) drops the sources registered
//! in it, except those [`pin_since`] keeps for a parse that outlives it
//! (the loader's cache), which [`remove`] drops when the file changes.

use crate::ast::Span;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

mod render;

struct Source {
    name: String,
    text: Arc<str>,
    /// Byte offset of every line's start, for `location`.
    lines: Arc<[usize]>,
}

struct Registry {
    sources: BTreeMap<u32, Source>,
    /// The id the next source gets; ids are never reused.
    next: u32,
    pinned: BTreeSet<u32>,
}

static SOURCES: Mutex<Registry> = Mutex::new(Registry {
    sources: BTreeMap::new(),
    next: 1,
    pinned: BTreeSet::new(),
});

/// Register a source; the id goes in every `Span` into it.
pub fn add_source(name: &str, text: &str) -> u32 {
    let mut s = SOURCES.lock().unwrap();
    let lines = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let id = s.next;
    s.next += 1;
    s.sources.insert(
        id,
        Source {
            name: name.to_string(),
            text: text.into(),
            lines,
        },
    );
    id
}

/// The id the next source will get: what [`pin_since`] and a [`Scope`]
/// count from.
pub fn mark() -> u32 {
    SOURCES.lock().unwrap().next
}

/// Keep the sources registered since `mark` beyond any scope; their ids.
pub fn pin_since(mark: u32) -> Vec<u32> {
    let mut s = SOURCES.lock().unwrap();
    let ids: Vec<u32> = s.sources.range(mark..).map(|(id, _)| *id).collect();
    s.pinned.extend(ids.iter().copied());
    ids
}

/// Drop sources nothing refers to any more.
pub fn remove(ids: &[u32]) {
    let mut s = SOURCES.lock().unwrap();
    for id in ids {
        s.sources.remove(id);
        s.pinned.remove(id);
    }
}

/// How many sources are registered.
pub fn source_count() -> usize {
    SOURCES.lock().unwrap().sources.len()
}

/// The sources of one evaluation: dropped with it, except the pinned.
pub struct Scope {
    mark: u32,
}

impl Scope {
    pub fn new() -> Scope {
        Scope { mark: mark() }
    }
}

impl Default for Scope {
    fn default() -> Scope {
        Scope::new()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let mut s = SOURCES.lock().unwrap();
        let s = &mut *s;
        let pinned = &s.pinned;
        s.sources
            .retain(|id, _| *id < self.mark || pinned.contains(id));
    }
}

fn source(id: u32) -> Option<(String, Arc<str>)> {
    let s = SOURCES.lock().unwrap();
    let src = s.sources.get(&id)?;
    Some((src.name.clone(), src.text.clone()))
}

fn source_lines(id: u32) -> Option<(String, Arc<str>, Arc<[usize]>)> {
    let s = SOURCES.lock().unwrap();
    let src = s.sources.get(&id)?;
    Some((src.name.clone(), src.text.clone(), src.lines.clone()))
}

/// The source a span is into: its name and its whole text.
pub fn source_of(span: Span) -> Option<(String, Arc<str>)> {
    source(span.file)
}

/// `(file, line, col)` of a span's start, 1-based, columns in characters.
pub fn location(span: Span) -> Option<(String, usize, usize)> {
    let (name, text, lines) = source_lines(span.file)?;
    let start = (span.start as usize).min(text.len());
    // The last line starting at or before `start`.
    let line = lines.partition_point(|&l| l <= start);
    let col = text[lines[line - 1]..start].chars().count() + 1;
    Some((name, line, col))
}

/// The span a place said as text names (`file:line:col`, as [`at`] says
/// one, or `file:line`): from its column (else the line's first word) to
/// the end of the line's text. A site printed in a message, a conflict's
/// witness's, found again.
pub fn span_at(place: &str) -> Option<Span> {
    let (rest, last) = place.rsplit_once(':')?;
    let last: usize = last.parse().ok()?;
    let (name, line, col) = match rest.rsplit_once(':') {
        Some((name, l)) if l.bytes().all(|b| b.is_ascii_digit()) => {
            (name, l.parse().ok()?, Some(last))
        }
        _ => (rest, last, None),
    };
    let s = SOURCES.lock().unwrap();
    let (&file, src) = s.sources.iter().rev().find(|(_, src)| src.name == name)?;
    let begin = *src.lines.get(line.checked_sub(1)?)?;
    let text = src.text[begin..]
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end();
    let start = match col {
        Some(c) => text
            .char_indices()
            .nth(c.checked_sub(1)?)
            .map_or(text.len(), |(i, _)| i),
        None => text.len() - text.trim_start().len(),
    };
    Some(Span {
        file,
        start: (begin + start) as u32,
        end: (begin + text.len()) as u32,
        origin: 0,
    })
}

/// The first `needle` within `span`'s text, when it is there.
pub fn find_in(span: Span, needle: &str) -> Option<Span> {
    let (_, text) = source(span.file)?;
    let within = text.get(span.start as usize..span.end as usize)?;
    let at = span.start as usize + within.find(needle)?;
    Some(Span {
        start: at as u32,
        end: (at + needle.len()) as u32,
        ..span
    })
}

/// `file:line:col`, or nothing for a span the compiler made up.
pub fn at(span: Span) -> Option<String> {
    location(span).map(|(f, l, c)| format!("{f}:{l}:{c}"))
}

static ORIGINS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The id of an origin (`policy baseline`, `module network instance
/// main`), for `Span::origin`.
pub fn origin_id(name: &str) -> u32 {
    let mut o = ORIGINS.lock().unwrap();
    let i = match o.iter().position(|x| x == name) {
        Some(i) => i,
        None => {
            o.push(name.to_string());
            o.len() - 1
        }
    };
    i as u32 + 1
}

pub fn origin(span: Span) -> Option<String> {
    let o = ORIGINS.lock().unwrap();
    o.get((span.origin as usize).checked_sub(1)?).cloned()
}

/// Where a statement is: `file:line:col`, then the pack or module
/// instance it was lowered out of: `policies/baseline.df:10:3, policy
/// baseline`. Nothing for a statement the compiler wrote.
pub fn place(span: Span) -> Option<String> {
    let at = at(span)?;
    Some(match origin(span) {
        Some(o) => format!("{at}, {o}"),
        None => at,
    })
}

/// What a diagnostic is: the word its first line leads with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// The program, or what was given it, is wrong: `error`.
    #[default]
    Error,
    /// A deny (or another policy) refuses the plan: `refused`.
    Refused,
    /// Two writes of one attribute disagree: `conflict`.
    Conflict,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Error => "error",
            Kind::Refused => "refused",
            Kind::Conflict => "conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: Kind,
    pub span: Span,
    pub message: String,
    /// What the caret under `span` says (`checked here`); none: the
    /// caret alone.
    pub label: String,
    /// Secondary spans with their own message.
    pub labels: Vec<(Span, String)>,
    /// Sites in no file, each its text and what it says: `--set
    /// agents=4`, `given here`.
    pub given: Vec<(String, String)>,
    /// `span` is said on the first line, after the message, not as a
    /// site under it: a deny's (the policy block's line).
    pub inline: bool,
    pub notes: Vec<String>,
    pub help: Option<String>,
    /// What an editor may apply to fix it (the language server's quick
    /// fixes).
    pub fixes: Vec<Fix>,
}

/// A fix a diagnostic suggests: edits, each the text that replaces a
/// span's bytes (an empty span inserts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<(Span, String)>,
}

impl Diagnostic {
    pub fn new(kind: Kind, span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            kind,
            span,
            message: message.into(),
            label: String::new(),
            labels: Vec::new(),
            given: Vec::new(),
            inline: false,
            notes: Vec::new(),
            help: None,
            fixes: Vec::new(),
        }
    }

    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic::new(Kind::Error, span, message)
    }

    /// A message with no site: said on one line by the same printer.
    pub fn bare(kind: Kind, message: impl Into<String>) -> Self {
        Diagnostic::new(kind, Span::default(), message)
    }

    /// What the caret under the primary span says.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push((span, message.into()));
        self
    }

    /// A site in no file: `--set agents=4`, `given here`.
    pub fn with_given(mut self, text: impl Into<String>, label: impl Into<String>) -> Self {
        self.given.push((text.into(), label.into()));
        self
    }

    /// The primary span said on the first line, not as a site.
    pub fn inline(mut self) -> Self {
        self.inline = true;
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_fix(mut self, title: impl Into<String>, edits: Vec<(Span, String)>) -> Self {
        self.fixes.push(Fix {
            title: title.into(),
            edits,
        });
        self
    }

    /// Its code, when its message leads with one (`E0304: ..`), and the
    /// message without it.
    pub fn code(&self) -> (Option<&str>, &str) {
        match self.message.split_once(": ") {
            Some((c, rest))
                if c.len() == 5
                    && c.starts_with('E')
                    && c[1..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                (Some(c), rest)
            }
            _ => (None, &self.message),
        }
    }

    /// As the terminal shows it ([`render`]): the kind word and the
    /// message, the sites as a tree, notes dim, the help last.
    pub fn render(&self, color: bool) -> String {
        render::render(self, crate::report::Style { color })
    }
}

/// Plain one line per diagnostic: `file:line:col: message`, then the hint.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match at(self.span) {
            Some(at) => write!(f, "{at}: {}", self.message)?,
            None => write!(f, "{}", self.message)?,
        }
        for (s, m) in &self.labels {
            if let Some(at) = at(*s) {
                write!(f, "\n  {at}: {m}")?;
            }
        }
        for n in &self.notes {
            write!(f, "\n  note: {n}")?;
        }
        if let Some(h) = &self.help {
            write!(f, "\n  help: {h}")?;
        }
        Ok(())
    }
}

/// One or more diagnostics as an error: what a load that fails returns.
#[derive(Debug, Clone)]
pub struct Diagnostics(pub Vec<Diagnostic>);

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostics {}

impl Diagnostics {
    /// Each as the terminal shows it, a blank line between; more than
    /// one, counted after them.
    pub fn render(&self, color: bool) -> String {
        let shown: Vec<String> = self.0.iter().map(|d| d.render(color)).collect();
        let mut out = shown.join("\n");
        if let n @ 2.. = self.0.len() {
            out.push_str(&format!("{n} errors\n"));
        }
        out
    }
}

/// Print an error: diagnostics as they render, anything else as one
/// with no site, in the one shape ([`shape`]). Returns the text printed.
pub fn report(err: &anyhow::Error, color: bool) -> String {
    if let Some(d) = err.chain().find_map(|e| e.downcast_ref::<Diagnostics>()) {
        return d.render(color);
    }
    // A provider's refusal of a change: at the resource's block.
    if let Some(d) = err
        .downcast_ref::<crate::report::Failure>()
        .and_then(|f| f.diagnostic())
    {
        return d.render(color);
    }
    Diagnostic::bare(Kind::Error, shape(err)).render(color)
}

/// An error in the one shape every error has (R-109): what happened on
/// its first line, then what caused it, each cause on its own lines
/// indented under it, never anyhow's `Caused by:` list. A cause the line
/// before already ends with (a context that says its cause) is said
/// once.
pub fn shape(err: &anyhow::Error) -> String {
    let mut chain = err.chain().map(|e| e.to_string());
    let mut out = chain.next().unwrap_or_default();
    let mut last = out.clone();
    for cause in chain {
        if !last.ends_with(&cause) {
            for line in cause.lines().filter(|l| !l.trim().is_empty()) {
                out.push_str("\n    ");
                out.push_str(line);
            }
        }
        last = cause;
    }
    out
}

/// The edit distance between `a` and `b`, by characters (Levenshtein).
pub fn edits(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// The one of `names` nearest `name` when one is near (a slip of the
/// pen): within a third of its length in edits, not `name` itself.
pub fn nearest<'a>(name: &str, names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names
        .into_iter()
        .map(|n| (edits(name, n), n))
        .filter(|(d, _)| *d > 0 && *d <= name.chars().count() / 3)
        .min()
        .map(|(_, n)| n)
}
