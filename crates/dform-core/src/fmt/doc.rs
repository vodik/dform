//! A document of text and possible line breaks, and its printer (Wadler's
//! "prettier printer", as prettier implements it): a group prints on one
//! line when the rest of that line fits the width, else its line breaks
//! all break. Groups are decided from the outside in: an outer group
//! breaks before an inner one, and an inner group that then fits stays on
//! one line. A group is measured up to the first place the rest of its
//! line could break, so of two groups on a line the later one breaks
//! first (prettier's rule); but a statement's own groups (its block, its
//! body) are measured together, so its block breaks before its body.

/// One piece of a document.
pub enum Doc {
    Text(String),
    Concat(Vec<Doc>),
    /// Its lines one step deeper, when its group breaks (a group on one
    /// line breaks inside only where a forced break crossed a barrier:
    /// that is indented from the line, not from the group).
    Indent(Box<Doc>),
    /// Breaks all its own lines, or none. `force`: it must break (it holds
    /// a comment, a hard line, a string of several lines). `stmt`: a
    /// statement's own group, not a term's. `barrier`: a forced break
    /// inside it does not force it (a body whose literal spans lines can
    /// stay on its line).
    Group {
        doc: Box<Doc>,
        force: bool,
        stmt: bool,
        barrier: bool,
    },
    /// A line break, or `flat` when its group is on one line. `hard`: a
    /// break whatever the group, which breaks every group around it.
    /// `blank`: an empty line before it, when it breaks.
    Line {
        flat: &'static str,
        hard: bool,
        blank: bool,
    },
    /// A line break unless the line holds nothing but indentation yet.
    Fresh,
    /// The first when its group breaks, the second when it does not.
    IfBreak(Box<Doc>, Box<Doc>),
    /// Text printed at the end of the line, before its break: a comment
    /// after code.
    Suffix(String),
    /// Breaks every group around it.
    BreakParent,
}

pub fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}

pub fn concat(v: Vec<Doc>) -> Doc {
    Doc::Concat(v)
}

pub fn nil() -> Doc {
    Doc::Concat(Vec::new())
}

pub fn indent(d: Doc) -> Doc {
    Doc::Indent(Box::new(d))
}

/// A term's group.
pub fn group(d: Doc, force: bool) -> Doc {
    Doc::Group {
        doc: Box::new(d),
        force,
        stmt: false,
        barrier: false,
    }
}

/// A statement's group: its block or its body (`barrier`, see
/// [`Doc::Group`]).
pub fn stmt_group(d: Doc, force: bool, barrier: bool) -> Doc {
    Doc::Group {
        doc: Box::new(d),
        force,
        stmt: true,
        barrier,
    }
}

/// A break, or `flat` on one line.
pub fn line(flat: &'static str) -> Doc {
    Doc::Line {
        flat,
        hard: false,
        blank: false,
    }
}

/// A break always; `blank`: after an empty line.
pub fn hard(blank: bool) -> Doc {
    Doc::Line {
        flat: "",
        hard: true,
        blank,
    }
}

pub fn if_break(broken: Doc, flat: Doc) -> Doc {
    Doc::IfBreak(Box::new(broken), Box::new(flat))
}

/// Whether `d` holds a forced break.
pub fn has_break(d: &Doc) -> bool {
    match d {
        Doc::Text(s) => s.contains('\n'),
        Doc::Concat(v) => v.iter().any(has_break),
        Doc::Indent(d) => has_break(d),
        Doc::Group { doc, force, .. } => *force || has_break(doc),
        Doc::Line { hard, .. } => *hard,
        Doc::Fresh | Doc::BreakParent => true,
        Doc::IfBreak(_, f) => has_break(f),
        Doc::Suffix(_) => false,
    }
}

/// Mark every group that holds a forced break as broken; whether `d`
/// holds one. The broken side of an `IfBreak` forces nothing outside it.
pub fn propagate(d: &mut Doc) -> bool {
    match d {
        Doc::Text(s) => s.contains('\n'),
        Doc::Concat(v) => v.iter_mut().fold(false, |acc, d| propagate(d) | acc),
        Doc::Indent(d) => propagate(d),
        Doc::Group {
            doc,
            force,
            barrier,
            ..
        } => {
            let inner = propagate(doc);
            if !*barrier {
                *force |= inner;
            }
            *force || inner
        }
        Doc::Line { hard, .. } => *hard,
        Doc::Fresh | Doc::BreakParent => true,
        Doc::IfBreak(b, f) => {
            propagate(b);
            propagate(f)
        }
        Doc::Suffix(_) => false,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Break,
}

const INDENT: &str = "  ";

fn width_of(s: &str) -> usize {
    s.chars().count()
}

/// Whether the group `next` (a statement's: `stmt`) fits in `left`
/// columns on one line, with what follows it to the end of its line
/// (`rest`, a stack: its last element comes first).
fn fits(next: &Doc, stmt: bool, rest: &[(usize, Mode, &Doc)], left: usize) -> bool {
    let mut left = left as isize;
    // Each piece, its mode, and whether it follows `next`.
    let mut cmds: Vec<(Mode, &Doc, bool)> = vec![(Mode::Flat, next, false)];
    let mut rest_idx = rest.len();
    loop {
        if left < 0 {
            return false;
        }
        let Some((mode, d, after)) = cmds.pop() else {
            if rest_idx == 0 {
                return true;
            }
            rest_idx -= 1;
            cmds.push((rest[rest_idx].1, rest[rest_idx].2, true));
            continue;
        };
        match d {
            Doc::Text(s) => match s.split_once('\n') {
                Some((first, _)) => return left >= width_of(first) as isize,
                None => left -= width_of(s) as isize,
            },
            Doc::Concat(v) => cmds.extend(v.iter().rev().map(|d| (mode, d, after))),
            Doc::Indent(d) => cmds.push((mode, d, after)),
            // A group after `next` on its line may break where it starts
            // (prettier's rule: the later group breaks), but for a
            // statement's group after a statement's group, measured on
            // one line: a block breaks before the body after it.
            Doc::Group {
                doc,
                force,
                stmt: g_stmt,
                ..
            } => {
                let flat = !*force && (!after || (stmt && *g_stmt) || mode == Mode::Flat);
                let m = if flat { Mode::Flat } else { Mode::Break };
                cmds.push((m, doc, after));
            }
            Doc::Line { flat, hard, .. } => {
                if *hard || mode == Mode::Break {
                    return true;
                }
                left -= width_of(flat) as isize;
            }
            Doc::Fresh => return true,
            Doc::IfBreak(b, f) => cmds.push((mode, if mode == Mode::Break { b } else { f }, after)),
            Doc::Suffix(_) | Doc::BreakParent => {}
        }
    }
}

/// The text printed so far.
#[derive(Default)]
struct Out {
    text: String,
    /// The column the text ends at, and where its last line begins.
    col: usize,
    line_start: usize,
    /// Trailing comments, printed before the next break.
    suffixes: Vec<String>,
}

impl Out {
    fn push(&mut self, s: &str) {
        self.text.push_str(s);
        match s.rfind('\n') {
            Some(i) => self.col = width_of(&s[i + 1..]),
            None => self.col += width_of(s),
        }
    }

    /// End the line (its comments first, its trailing spaces dropped), an
    /// empty one after it if `blank`, and indent the next `ind` steps.
    fn newline(&mut self, ind: usize, blank: bool) {
        self.end_line();
        if blank {
            self.text.push('\n');
        }
        self.text.push('\n');
        for _ in 0..ind {
            self.text.push_str(INDENT);
        }
        self.line_start = self.text.len();
        self.col = ind * INDENT.len();
    }

    fn end_line(&mut self) {
        for s in std::mem::take(&mut self.suffixes) {
            self.text.push_str(&s);
        }
        let trimmed = self.text.trim_end_matches(' ').len();
        self.text.truncate(trimmed);
    }
}

/// Print `d` in `width` columns. Run [`propagate`] on it first.
pub fn print(d: &Doc, width: usize) -> String {
    let mut out = Out::default();
    let mut cmds: Vec<(usize, Mode, &Doc)> = vec![(0, Mode::Break, d)];
    while let Some((ind, mode, d)) = cmds.pop() {
        match d {
            Doc::Text(s) => out.push(s),
            Doc::Concat(v) => cmds.extend(v.iter().rev().map(|d| (ind, mode, d))),
            Doc::Indent(d) => cmds.push((ind + usize::from(mode == Mode::Break), mode, d)),
            Doc::Group {
                doc: g,
                force,
                stmt,
                ..
            } => {
                let left = width.saturating_sub(out.col);
                let flat = !*force && (mode == Mode::Flat || fits(g, *stmt, &cmds, left));
                cmds.push((ind, if flat { Mode::Flat } else { Mode::Break }, g));
            }
            Doc::Line { flat, hard, blank } => {
                if mode == Mode::Flat && !*hard {
                    out.push(flat);
                } else {
                    out.newline(ind, *blank);
                }
            }
            Doc::Fresh => {
                if !out.text[out.line_start..].trim().is_empty() {
                    out.newline(ind, false);
                }
            }
            Doc::IfBreak(b, f) => cmds.push((ind, mode, if mode == Mode::Break { b } else { f })),
            Doc::Suffix(s) => out.suffixes.push(s.clone()),
            Doc::BreakParent => {}
        }
    }
    out.end_line();
    out.text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Doc {
        let mut inner = Vec::new();
        for (i, it) in items.iter().enumerate() {
            inner.push(line(""));
            inner.push(text(*it));
            inner.push(if i + 1 < items.len() {
                text(",")
            } else {
                if_break(text(","), nil())
            });
            if i + 1 < items.len() {
                inner.push(text(" "));
            }
        }
        group(
            concat(vec![text("["), indent(concat(inner)), line(""), text("]")]),
            false,
        )
    }

    #[test]
    fn a_group_breaks_only_when_its_line_does_not_fit() {
        let mut d = list(&["aa", "bb"]);
        propagate(&mut d);
        assert_eq!(print(&d, 20), "[aa, bb]");
        assert_eq!(print(&d, 5), "[\n  aa,\n  bb,\n]");
    }

    #[test]
    fn a_suffix_goes_before_the_break() {
        let mut d = group(
            concat(vec![
                text("{"),
                Doc::Suffix(" # c".into()),
                Doc::BreakParent,
                indent(concat(vec![line(" "), text("a")])),
                line(" "),
                text("}"),
            ]),
            false,
        );
        propagate(&mut d);
        assert_eq!(print(&d, 80), "{ # c\n  a\n}");
    }
}
