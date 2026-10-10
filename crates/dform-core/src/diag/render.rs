//! A diagnostic as the terminal shows it, in the plan's grammar and `why
//! --tree`'s: the kind word and the sentence on the first line (it reads
//! alone, a log line), its code dim at the end; each site hung off the
//! tree's glyphs, `file:line` in the site column and the source line
//! beside it, a caret under the span when it is narrower than the line;
//! notes dim under the tree, the help last. A diagnostic with one site
//! draws no glyph; one with none is its first line.

use super::{Diagnostic, location, source_lines};
use crate::ast::Span;
use crate::report::tree::branch;
use crate::report::{Paint, Style};

/// The rows an excerpt keeps of a span over more lines: its first two
/// and its last, the rest elided.
const LINES: usize = 4;

/// One entry of the tree: where, the source there, the caret under the
/// span in its last row.
struct Site {
    place: String,
    excerpt: Vec<String>,
    /// The caret's column in the last row of the excerpt and its width.
    caret: Option<(usize, usize)>,
    label: String,
}

impl Site {
    /// The site of `span`, unless the compiler made it up.
    fn of(span: Span, label: &str) -> Option<Site> {
        let (name, text, lines) = source_lines(span.file)?;
        let name = relative(&name);
        let end = (span.end as usize).min(text.len());
        let start = (span.start as usize).min(end);
        let line_of = |at: usize| lines.partition_point(|&l| l <= at);
        let (first, last) = (line_of(start), line_of(end.saturating_sub(1).max(start)));
        let line = |n: usize| text[lines[n - 1]..].split('\n').next().unwrap_or("");
        let indent = |l: &str| l.len() - l.trim_start().len();
        let label = label.to_string();
        if first == last {
            let raw = line(first).trim_end();
            let lead = lines[first - 1] + indent(raw);
            let shown = raw.trim_start();
            let col = text
                .get(lead..start.max(lead))
                .map_or(0, |s| s.chars().count());
            let width = text[start..end.min(lead + shown.len()).max(start)]
                .chars()
                .count();
            let whole = col == 0 && width >= shown.chars().count();
            return Some(Site {
                place: format!("{name}:{first}"),
                excerpt: vec![shown.to_string()],
                caret: (!whole).then_some((col, width.max(1))),
                label,
            });
        }
        let all: Vec<&str> = (first..=last).map(|n| line(n).trim_end()).collect();
        let cut = all
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| indent(l))
            .min()
            .unwrap_or(0);
        let dedent = |l: &str| l.get(cut..).unwrap_or("").to_string();
        let mut excerpt: Vec<String> = match all.len() > LINES {
            true => vec![dedent(all[0]), dedent(all[1]), "...".into()],
            false => all.iter().map(|l| dedent(l)).collect(),
        };
        while excerpt.last().is_some_and(|l| l.is_empty()) {
            excerpt.pop();
        }
        // Its end, under the last row shown.
        let col = text[lines[last - 1]..end]
            .chars()
            .count()
            .saturating_sub(cut);
        Some(Site {
            place: format!("{name}:{first}-{last}"),
            excerpt,
            caret: (!label.is_empty()).then_some((col.saturating_sub(1), 1)),
            label,
        })
    }

    /// A site in no file: its text in the site column.
    fn given(text: &str, label: &str) -> Site {
        Site {
            place: text.to_string(),
            excerpt: Vec::new(),
            caret: None,
            label: label.to_string(),
        }
    }

    /// Its rows: `lead` before the first, `under` before the rest, the
    /// site column `width` wide.
    fn write(&self, out: &mut Vec<String>, lead: &str, under: &str, width: usize, style: Style) {
        let pad = " ".repeat(width.saturating_sub(self.place.chars().count()));
        let column = " ".repeat(width);
        let label = |s: &str| style.paint(Paint::Bold, s);
        let mut rows = self.excerpt.clone();
        // A label with no caret to hang off follows the source line.
        if self.caret.is_none() && !self.label.is_empty() {
            match rows.last_mut() {
                Some(l) => *l = format!("{l}  {}", label(&self.label)),
                None => rows.push(label(&self.label)),
            }
        }
        let place = style.paint(Paint::Dim, &self.place);
        for (i, row) in rows.iter().enumerate() {
            match i {
                0 => out.push(format!("{lead}{place}{pad}  {row}")),
                _ => out.push(format!("{under}{column}  {row}")),
            }
        }
        if rows.is_empty() {
            out.push(format!("{lead}{place}"));
        }
        if let Some((col, n)) = self.caret {
            let carets = style.paint(Paint::Because, &"^".repeat(n));
            let label = match self.label.is_empty() {
                true => String::new(),
                false => format!(" {}", label(&self.label)),
            };
            out.push(format!(
                "{under}{column}  {}{carets}{label}",
                " ".repeat(col)
            ));
        }
    }
}

/// A source's name as the reader writes it: a path under the working
/// directory relative to it, as the plan's site column says one.
fn relative(name: &str) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return name.to_string();
    };
    match std::path::Path::new(name).strip_prefix(&cwd) {
        Ok(rest) => rest.display().to_string(),
        Err(_) => name.to_string(),
    }
}

/// `d` as the terminal shows it, painted in `style`.
pub(super) fn render(d: &Diagnostic, style: Style) -> String {
    let (code, sentence) = d.code();
    let mut said = sentence.lines();
    let mut head = format!(
        "{}  {}",
        style.paint(Paint::Error, d.kind.word()),
        said.next().unwrap_or("")
    );
    let mut sites: Vec<Site> = Vec::new();
    // A site on the first line makes what is under it a tree.
    let mut tree = false;
    match d.inline {
        true => {
            if let Some((f, l, _)) = location(d.span) {
                let at = format!("{}:{l}", relative(&f));
                head.push_str(&format!("  {}", style.paint(Paint::Dim, &at)));
                tree = true;
            }
        }
        false => sites.extend(Site::of(d.span, &d.label)),
    }
    if let Some(c) = code {
        head.push_str(&format!("  {}", style.paint(Paint::Dim, &format!("[{c}]"))));
    }
    let mut out = vec![head];
    out.extend(said.map(str::to_string));
    sites.extend(d.labels.iter().filter_map(|(s, l)| Site::of(*s, l)));
    sites.extend(d.given.iter().map(|(t, l)| Site::given(t, l)));
    let width = sites
        .iter()
        .map(|s| s.place.chars().count())
        .max()
        .unwrap_or(0);
    for (i, s) in sites.iter().enumerate() {
        let (lead, under) = match sites.len() {
            1 if !tree => ("  ".to_string(), "  ".to_string()),
            n => {
                let (b, u) = branch(i, n);
                (format!("  {b}"), format!("  {u}"))
            }
        };
        s.write(&mut out, &lead, &under, width, style);
    }
    for n in &d.notes {
        out.extend(
            n.lines()
                .map(|l| format!("  {}", style.paint(Paint::Note, l))),
        );
    }
    if let Some(h) = &d.help {
        let mut lines = h.lines();
        out.push(format!("  help: {}", lines.next().unwrap_or("")));
        out.extend(lines.map(|l| format!("        {l}")));
    }
    let mut text: String = out
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    text
}
