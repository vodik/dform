//! The manual (R-148): dform(1) and a page per command, dform-plan(1),
//! dform-stack-list(1), made by clap_mangen from the command line's
//! definitions (NAME, SYNOPSIS, DESCRIPTION, OPTIONS, SUBCOMMANDS), and
//! dform(1)'s hand-written sections, the fragments under docs/man/, which
//! docs/reference.md carries too ([`splice`]). `cargo xtask man` writes
//! the pages to target/man and the fragments into docs/reference.md.

use anyhow::{Result, bail};
use clap_mangen::Man;
use clap_mangen::roff::{Roff, bold, roman};

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
/// command before its subcommands. clap's `help` subcommand, which every
/// command with subcommands has, prints `--help` and has no page.
pub fn pages() -> Vec<Page> {
    fn walk(cmd: &clap::Command, parent: Option<&str>, out: &mut Vec<Page>) {
        out.push(render(cmd, parent));
        let name = cmd.get_display_name().unwrap_or(cmd.get_name());
        for sub in cmd
            .get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        {
            walk(sub, Some(name), out);
        }
    }
    let mut out = Vec::new();
    walk(&tree(), None, &mut out);
    out
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

#[cfg(test)]
mod tests {
    use super::*;

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
