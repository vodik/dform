//! Diagnostics: a message at a span, with labels, notes and a hint, printed
//! through ariadne. Sources are registered once per load so a `Span` (a
//! source id and a byte range) can name `file:line:col` anywhere later.

use crate::ast::Span;
use std::fmt;
use std::sync::{Arc, Mutex};

struct Source {
    name: String,
    text: Arc<str>,
}

static SOURCES: Mutex<Vec<Source>> = Mutex::new(Vec::new());

/// Register a source; the id goes in every `Span` into it.
pub fn add_source(name: &str, text: &str) -> u32 {
    let mut s = SOURCES.lock().unwrap();
    s.push(Source {
        name: name.to_string(),
        text: text.into(),
    });
    s.len() as u32
}

fn source(id: u32) -> Option<(String, Arc<str>)> {
    let s = SOURCES.lock().unwrap();
    let src = s.get((id as usize).checked_sub(1)?)?;
    Some((src.name.clone(), src.text.clone()))
}

/// `(file, line, col)` of a span's start, 1-based, columns in characters.
pub fn location(span: Span) -> Option<(String, usize, usize)> {
    let (name, text) = source(span.file)?;
    let start = (span.start as usize).min(text.len());
    let before = &text[..start];
    let line = before.matches('\n').count() + 1;
    let col = before[before.rfind('\n').map_or(0, |i| i + 1)..]
        .chars()
        .count()
        + 1;
    Some((name, line, col))
}

/// `file:line:col`, or nothing for a span the compiler made up.
pub fn at(span: Span) -> Option<String> {
    location(span).map(|(f, l, c)| format!("{f}:{l}:{c}"))
}

/// ` (file:line:col)` to append to a message, or nothing.
pub fn suffix(span: Span) -> String {
    at(span).map(|s| format!(" ({s})")).unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
    /// Secondary spans with their own message.
    pub labels: Vec<(Span, String)>,
    pub notes: Vec<String>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            span,
            message: message.into(),
            labels: Vec::new(),
            notes: Vec::new(),
            help: None,
        }
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push((span, message.into()));
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

    /// The ariadne report: the source line, the label, notes and the hint.
    pub fn render(&self, color: bool) -> String {
        use ariadne::{Config, IndexType, Label, Report, ReportKind};
        let Some((name, text)) = source(self.span.file) else {
            return format!("error: {}\n", self.message);
        };
        let range = |s: Span| {
            let end = (s.end as usize).min(text.len());
            let start = (s.start as usize).min(end);
            start..end
        };
        let mut r = Report::build(ReportKind::Error, (name.clone(), range(self.span)))
            .with_config(
                Config::default()
                    .with_color(color)
                    .with_index_type(IndexType::Byte),
            )
            .with_message(format!(
                "{}: {}",
                at(self.span).unwrap_or_default(),
                self.message
            ))
            .with_label(Label::new((name.clone(), range(self.span))).with_message(&self.message));
        let mut sources = vec![(name.clone(), text.clone())];
        for (s, m) in &self.labels {
            if let Some((n, t)) = source(s.file) {
                if !sources.iter().any(|(x, _)| *x == n) {
                    sources.push((n.clone(), t));
                }
                let end =
                    (s.end as usize).min(sources.iter().find(|(x, _)| *x == n).unwrap().1.len());
                r = r.with_label(Label::new((n, (s.start as usize).min(end)..end)).with_message(m));
            }
        }
        for n in &self.notes {
            r = r.with_note(n);
        }
        if let Some(h) = &self.help {
            r = r.with_help(h);
        }
        let mut out = Vec::new();
        let cache = ariadne::sources(sources.into_iter().map(|(n, t)| (n, t.to_string())));
        r.finish().write(cache, &mut out).expect("write to a Vec");
        String::from_utf8_lossy(&out).into_owned()
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
    pub fn render(&self, color: bool) -> String {
        let mut out = String::new();
        for d in &self.0 {
            out.push_str(&d.render(color));
        }
        let n = self.0.len();
        out.push_str(&format!("{n} error{}\n", if n == 1 { "" } else { "s" }));
        out
    }
}

/// Print an error: diagnostics through ariadne, anything else as anyhow's
/// chain. Returns the text printed.
pub fn report(err: &anyhow::Error, color: bool) -> String {
    match err.chain().find_map(|e| e.downcast_ref::<Diagnostics>()) {
        Some(d) => d.render(color),
        None => format!("Error: {err:?}\n"),
    }
}
