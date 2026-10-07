//! The manual (R-148): dform(1) and a page per command, dform-plan(1),
//! dform-stack-list(1), made by clap_mangen from the command line's
//! definitions (NAME, SYNOPSIS, DESCRIPTION, OPTIONS, SUBCOMMANDS), and
//! dform(1)'s hand-written sections, the fragments under docs/man/, which
//! docs/reference.md carries too ([`splice`]). `cargo xtask man` writes
//! the pages to target/man and the fragments into docs/reference.md;
//! `dform help CMD` renders a page on the terminal ([`help`]).

use anyhow::{Result, bail};
use clap_mangen::Man;
use clap_mangen::roff::{Roff, bold, roman};
use std::io::Write;

/// A hand-written section of dform(1), one Markdown file under docs/man/
/// that docs/reference.md carries between its markers.
pub struct Fragment {
    /// Its file's stem and its markers' name: `exit-status`.
    pub name: &'static str,
    /// The page's section heading.
    pub heading: &'static str,
    pub markdown: &'static str,
}

/// dform(1)'s sections after the generated ones, in order.
pub const FRAGMENTS: [Fragment; 4] = [
    Fragment {
        name: "exit-status",
        heading: "EXIT STATUS",
        markdown: include_str!("../docs/man/exit-status.md"),
    },
    Fragment {
        name: "environment",
        heading: "ENVIRONMENT",
        markdown: include_str!("../docs/man/environment.md"),
    },
    Fragment {
        name: "files",
        heading: "FILES",
        markdown: include_str!("../docs/man/files.md"),
    },
    Fragment {
        name: "see-also",
        heading: "SEE ALSO",
        markdown: include_str!("../docs/man/see-also.md"),
    },
];

/// One manual page.
pub struct Page {
    /// `dform`, `dform-plan`, `dform-stack-list`: the file is `NAME.1`.
    pub name: String,
    /// The page's roff source.
    pub roff: String,
}

/// The command line's definitions, built: every command its page's name
/// (`dform-stack-list`) and its synopsis's (`dform stack list`).
fn tree() -> clap::Command {
    let mut cmd = crate::cli::command();
    cmd.build();
    cmd
}

/// Every page: dform(1), then each command's that `--help` lists, a
/// command before its subcommands.
pub fn pages() -> Vec<Page> {
    fn walk(cmd: &clap::Command, parent: Option<&str>, out: &mut Vec<Page>) {
        out.push(render(cmd, parent));
        let name = cmd.get_display_name().unwrap_or(cmd.get_name());
        for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
            walk(sub, Some(name), out);
        }
    }
    let mut out = Vec::new();
    walk(&tree(), None, &mut out);
    out
}

/// The page of the command `words` names (`["stack", "list"]`), dform(1)
/// for none.
pub fn page(words: &[String]) -> Result<Page> {
    let top = tree();
    let mut cmd = &top;
    let mut parent = None;
    for (i, w) in words.iter().enumerate() {
        match cmd.find_subcommand(w).filter(|s| !s.is_hide_set()) {
            Some(sub) => {
                parent = Some(cmd.get_display_name().unwrap_or(cmd.get_name()));
                cmd = sub;
            }
            None => bail!(
                "no command `dform {}`: `dform help` lists the commands",
                words[..=i].join(" ")
            ),
        }
    }
    Ok(render(cmd, parent))
}

/// `cmd`'s page: clap_mangen's sections, then dform(1)'s fragments, or a
/// command's SEE ALSO (dform(1) and the command it is under).
fn render(cmd: &clap::Command, parent: Option<&str>) -> Page {
    // `--help`'s closing line points at this page.
    let cmd = cmd.clone().after_help(None::<&'static str>);
    let name = cmd.get_display_name().unwrap_or(cmd.get_name()).to_string();
    let man = Man::new(cmd)
        .title(name.to_uppercase())
        // No date (a page is the same for every build of a version), but
        // its argument, so the source and the manual keep their places.
        .date("\"\"")
        .source(format!("dform {}", env!("CARGO_PKG_VERSION")))
        .manual("dform Manual");
    let mut out = Vec::new();
    man.render(&mut out).expect("writing to a Vec");
    let mut out = code(&String::from_utf8(out).expect("roff of UTF-8 is UTF-8")).into_bytes();
    let mut roff = Roff::new();
    match parent {
        None => {
            for f in &FRAGMENTS {
                roff.control("SH", [f.heading]);
                markdown(f.markdown, &mut roff);
            }
        }
        Some(parent) => {
            roff.control("SH", ["SEE ALSO"]);
            let mut line = vec![bold("dform"), roman("(1)")];
            if parent != "dform" {
                line.extend([roman(", "), bold(parent), roman("(1)")]);
            }
            roff.text(line);
        }
    }
    // Each rendering begins with the same preamble (the apostrophe's
    // string); the page needs it once.
    let preamble = Roff::new().render();
    let sections = roff.render();
    out.extend_from_slice(
        sections
            .strip_prefix(&preamble)
            .unwrap_or(&sections)
            .as_bytes(),
    );
    Page {
        name,
        roff: String::from_utf8(out).expect("roff of UTF-8 is UTF-8"),
    }
}

/// The help texts' Markdown code spans (`dform plan`) in bold: each
/// paragraph's (the text lines between two control lines), when its
/// backticks pair up.
fn code(roff: &str) -> String {
    let mut out = String::new();
    let mut para: Vec<&str> = Vec::new();
    let end = |para: &mut Vec<&str>, out: &mut String| {
        let ticks: usize = para.iter().map(|l| l.matches('`').count()).sum();
        let mut open = false;
        for line in para.drain(..) {
            if ticks % 2 == 1 {
                out.push_str(line);
            } else {
                for (i, part) in line.split('`').enumerate() {
                    if i > 0 {
                        out.push_str(if open { "\\fR" } else { "\\fB" });
                        open = !open;
                    }
                    out.push_str(part);
                }
            }
            out.push('\n');
        }
    };
    for line in roff.lines() {
        if line.starts_with('.') {
            end(&mut para, &mut out);
            out.push_str(line);
            out.push('\n');
        } else {
            para.push(line);
        }
    }
    end(&mut para, &mut out);
    out
}

/// A fragment's Markdown as roff: paragraphs, and two-column tables as
/// tagged paragraphs (the first column the tag); `code` is bold.
fn markdown(md: &str, roff: &mut Roff) {
    for (i, block) in md.trim().split("\n\n").enumerate() {
        let lines: Vec<&str> = block.lines().map(str::trim).collect();
        if lines.iter().all(|l| l.starts_with('|')) {
            // The header and its rule are the reference's, not the page's.
            for row in lines.iter().skip(2) {
                let cells = cells(row);
                roff.control("TP", []);
                roff.text(inline(cells.first().map_or("", String::as_str)));
                roff.text(inline(&cells[1..].join(" ")));
            }
        } else {
            if i > 0 {
                roff.control("PP", []);
            }
            roff.text(inline(&lines.join(" ")));
        }
    }
}

/// A table row's cells, `\|` a bar inside one.
fn cells(row: &str) -> Vec<String> {
    let row = row.trim().trim_start_matches('|');
    let row = row.strip_suffix('|').unwrap_or(row);
    let mut cells = vec![String::new()];
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cells.last_mut().unwrap().push(chars.next().unwrap())
            }
            '|' => cells.push(String::new()),
            c => cells.last_mut().unwrap().push(c),
        }
    }
    cells.iter().map(|c| c.trim().to_string()).collect()
}

/// Markdown's inline text: `code` bold, the rest roman.
fn inline(text: &str) -> Vec<clap_mangen::roff::Inline> {
    text.split('`')
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| if i % 2 == 1 { bold(s) } else { roman(s) })
        .collect()
}

/// The markers around a fragment's copy in docs/reference.md.
fn markers(name: &str) -> (String, String) {
    (
        format!("<!-- man:{name}: docs/man/{name}.md, copied by `cargo xtask man` -->"),
        format!("<!-- /man:{name} -->"),
    )
}

/// `reference` (docs/reference.md) with each fragment between its
/// markers: the same text when it carries them as they are. A missing
/// marker is an error.
pub fn splice(reference: &str) -> Result<String> {
    let mut out = reference.to_string();
    for f in &FRAGMENTS {
        let (begin, end) = markers(f.name);
        let Some(b) = out.find(&begin) else {
            bail!("docs/reference.md has no `{begin}` line");
        };
        let from = b + begin.len();
        let Some(e) = out[from..].find(&end) else {
            bail!("docs/reference.md has no `{end}` line after `{begin}`");
        };
        out.replace_range(from..from + e, &format!("\n{}\n", f.markdown.trim()));
    }
    Ok(out)
}

/// `dform help [COMMAND..]`: the page, formatted for the terminal (of
/// `width` columns, when stdout is one) and given to `$MANPAGER` or
/// `$PAGER`, as man(1) does; printed as plain text otherwise.
pub fn help(words: &[String], width: Option<usize>) -> Result<()> {
    let page = page(words)?;
    let pager = width
        .and_then(|_| {
            ["MANPAGER", "PAGER"]
                .iter()
                .find_map(|v| std::env::var(v).ok())
        })
        .filter(|p| !p.trim().is_empty());
    let width = width.map_or(80, |w| w.saturating_sub(1).max(40));
    let out = match pager {
        Some(pager) => {
            let text = text(&page.roff, width, true);
            // The operator's pager, as they wrote it (`less -R`, a
            // pipeline): the one other command dform runs because it was
            // asked to, beside the audit sink.
            match std::process::Command::new("sh")
                .args(["-c", &pager])
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    let mut stdin = child.stdin.take().expect("piped");
                    // A pager quit before the end closes the pipe.
                    let _ = stdin.write_all(text.as_bytes());
                    drop(stdin);
                    child.wait()?;
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("warning: pager {pager:?}: {e}; printing the page");
                    text
                }
            }
        }
        None => text(&page.roff, width, false),
    };
    match std::io::stdout().lock().write_all(out.as_bytes()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Font {
    Roman,
    Bold,
    Italic,
}

/// A run of characters in their fonts.
type Run = Vec<(char, Font)>;

/// A non-breaking space (`\ `): a space inside a word.
const NBSP: char = '\u{a0}';

/// `roff` formatted as text `width` columns wide, with the man(7) macros
/// clap_mangen and [`markdown`] write (SH, PP, TP, IP, RS, RE, br): what
/// nroff would make of it, minimally. `overstrike`: bold and italic as a
/// terminal pager shows them (`c\bc`, `_\bc`); else plain.
pub fn text(roff: &str, width: usize, overstrike: bool) -> String {
    let mut f = Fmt {
        width,
        overstrike,
        out: String::new(),
        margin: 0,
        indent: 0,
        stack: Vec::new(),
        tag: None,
        want_tag: None,
        words: Vec::new(),
        gap: false,
        font: Font::Roman,
        heading: false,
    };
    for line in roff.lines() {
        if let Some(control) = line.strip_prefix('.') {
            let (name, args) = control.split_once(' ').unwrap_or((control, ""));
            f.control(name.trim(), &arguments(args));
        } else if line.trim().is_empty() {
            // An empty text line is a blank line, as nroff makes it.
            f.flush();
            f.gap = true;
        } else if f.want_tag.is_some() {
            let n = f.want_tag.take().unwrap();
            f.tag = Some(join(&words(&escapes(line, &mut f.font))));
            f.indent = f.margin + n;
        } else {
            let run = escapes(line, &mut f.font);
            f.words.extend(words(&run));
        }
    }
    f.flush();
    f.out
}

struct Fmt {
    width: usize,
    overstrike: bool,
    out: String,
    /// The left margin: a section's body, moved by RS.
    margin: usize,
    /// Where the paragraph's lines start.
    indent: usize,
    /// The margins RS moved from.
    stack: Vec<usize>,
    /// The tag of the paragraph being filled (TP, IP).
    tag: Option<Run>,
    /// TP: the next text line is the tag, the body indented this much.
    want_tag: Option<usize>,
    /// The paragraph's words, to fill.
    words: Vec<Run>,
    /// A blank line before the next.
    gap: bool,
    /// The font a text line starts in: the one the line before left.
    font: Font,
    /// The last line was a heading: no blank line after it.
    heading: bool,
}

impl Fmt {
    fn control(&mut self, name: &str, args: &[String]) {
        let n = |i: usize, default: usize| {
            args.get(i)
                .and_then(|a| a.parse::<usize>().ok())
                .unwrap_or(default)
        };
        match name {
            "SH" | "SS" => {
                self.flush();
                self.gap = !self.out.is_empty();
                let heading: Run = escapes(&args.join(" "), &mut Font::Roman)
                    .into_iter()
                    .map(|(c, _)| (c, Font::Bold))
                    .collect();
                self.line(if name == "SH" { 0 } else { 3 }, &heading);
                self.heading = true;
                self.margin = 7;
                self.indent = 7;
                self.stack.clear();
            }
            "PP" | "LP" | "P" => {
                self.flush();
                self.gap = true;
                self.indent = self.margin;
            }
            "TP" => {
                self.flush();
                self.gap = true;
                self.want_tag = Some(n(0, 7));
            }
            "IP" => {
                self.flush();
                self.gap = true;
                self.tag = args
                    .first()
                    .map(|t| escapes(t, &mut Font::Roman))
                    .filter(|t| !t.is_empty());
                self.indent = self.margin + n(1, 7);
            }
            "RS" => {
                self.flush();
                self.stack.push(self.margin);
                self.margin += n(0, 7);
                self.indent = self.margin;
            }
            "RE" => {
                self.flush();
                self.margin = self.stack.pop().unwrap_or(7);
                self.indent = self.margin;
            }
            "br" => self.flush(),
            "sp" => {
                self.flush();
                self.gap = true;
            }
            // TH, the apostrophe preamble's ie/el/ds, and the rest.
            _ => {}
        }
    }

    /// Fill the paragraph's words (after its tag) to the width.
    fn flush(&mut self) {
        let words = std::mem::take(&mut self.words);
        let mut indent = self.indent;
        let mut current: Run = Vec::new();
        let mut fresh = true;
        if let Some(tag) = self.tag.take() {
            if words.is_empty() || self.margin + tag.len() + 1 > self.indent {
                self.line(self.margin, &tag);
            } else {
                current = tag;
                current.resize(self.indent - self.margin, (' ', Font::Roman));
                indent = self.margin;
            }
        }
        for w in words {
            let column = indent + current.len();
            if !fresh && column + 1 + w.len() > self.width {
                self.line(indent, &current);
                current.clear();
                indent = self.indent;
                fresh = true;
            }
            if !fresh {
                current.push((' ', Font::Roman));
            }
            current.extend(w);
            fresh = false;
        }
        if !fresh {
            self.line(indent, &current);
        }
    }

    fn line(&mut self, indent: usize, run: &[(char, Font)]) {
        if self.gap && !self.heading && !self.out.is_empty() {
            self.out.push('\n');
        }
        self.gap = false;
        self.heading = false;
        self.out.extend(std::iter::repeat_n(' ', indent));
        for &(c, font) in run {
            let c = if c == NBSP { ' ' } else { c };
            match font {
                Font::Bold if self.overstrike && c != ' ' => {
                    self.out.extend([c, '\u{8}', c]);
                }
                Font::Italic if self.overstrike && c != ' ' => {
                    self.out.extend(['_', '\u{8}', c]);
                }
                _ => self.out.push(c),
            }
        }
        self.out.push('\n');
    }
}

/// A control line's arguments: words, a quoted one with its spaces.
fn arguments(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = args.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
        } else if c == '"' {
            chars.next();
            out.push(chars.by_ref().take_while(|&c| c != '"').collect());
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek().filter(|&&c| c != ' ') {
                word.push(c);
                chars.next();
            }
            out.push(word);
        }
    }
    out
}

/// A text line's characters in their fonts, its escapes resolved; `font`
/// the one it starts in, then the one it ends in.
fn escapes(line: &str, font: &mut Font) -> Run {
    let mut out = Vec::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push((c, *font));
            continue;
        }
        let special = |name: &str| match name {
            "bu" => '\u{2022}',
            "aq" | "Aq" => '\'',
            "em" => '\u{2014}',
            "en" => '\u{2013}',
            "dq" => '"',
            _ => '?',
        };
        match chars.next() {
            Some('f') => {
                *font = match chars.next() {
                    Some('B') => Font::Bold,
                    Some('I') => Font::Italic,
                    _ => Font::Roman,
                }
            }
            Some('(') => {
                let name: String = chars.by_ref().take(2).collect();
                out.push((special(&name), *font));
            }
            Some('*') => {
                let name: String = match chars.next() {
                    Some('(') => chars.by_ref().take(2).collect(),
                    Some(c) => c.to_string(),
                    None => String::new(),
                };
                out.push((special(&name), *font));
            }
            Some(' ') => out.push((NBSP, *font)),
            Some('e') | Some('\\') => out.push(('\\', *font)),
            Some('&') | Some('%') | Some('c') | None => {}
            Some(c) => out.push((c, *font)),
        }
    }
    out
}

/// A run split at its spaces.
fn words(run: &[(char, Font)]) -> Vec<Run> {
    run.split(|&(c, _)| c == ' ')
        .filter(|w| !w.is_empty())
        .map(<[_]>::to_vec)
        .collect()
}

/// Words joined by a space.
fn join(words: &[Run]) -> Run {
    let mut out = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push((' ', Font::Roman));
        }
        out.extend(w.iter().copied());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_fills_and_tags() {
        let roff = ".SH NAME\nfoo \\- a thing\n.SH OPTIONS\n.TP\n\\fB\\-q\\fR\nquiet, and\nso on\n\
                    .TP\n\\fB\\-\\-a\\-long\\-flag\\fR \\fIV\\fR\nbody\n";
        assert_eq!(
            text(roff, 80, false),
            "NAME\n       foo - a thing\n\nOPTIONS\n       -q     quiet, and so on\n\n\
             \x20      --a-long-flag V\n              body\n"
        );
        assert_eq!(
            text(".SH X\n\\fBb\\fR", 80, true),
            "X\u{8}X\n       b\u{8}b\n"
        );
    }

    #[test]
    fn splice_replaces_between_markers() {
        let mut doc = String::new();
        for f in &FRAGMENTS {
            let (b, e) = markers(f.name);
            doc.push_str(&format!("# {}\n{b}\nSTALE\n{e}\n", f.name));
        }
        let spliced = splice(&doc).unwrap();
        assert!(!spliced.contains("STALE"));
        assert_eq!(splice(&spliced).unwrap(), spliced);
        assert!(splice("nothing").is_err());
    }
}
