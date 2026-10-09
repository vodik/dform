//! A value's chain as `why` and `-vv` print it (R-122): each step of how the value
//! was made, its expression whole or with long literals elided.

use super::Why;
use super::deformation::Line;
use super::layout::{Row, WIDTH, layout};
use super::mask::elide;
use super::style::Style;
use super::tree;

/// Under attribute line `l`, at `-vv`, how its value was made (R-122):
/// one `= EXPR   SITE` row per step, then `over EXPR   SITE` per write
/// it beat. A value stated as the literal it is says nothing more.
pub(super) fn write_chain(rows: &mut Vec<Row>, l: &Line, indent: &str, why: Why) {
    if let [only] = l.chain.as_slice()
        && !only.lost
        && only.with.is_empty()
        && only.expr == l.after.said(why)
    {
        return;
    }
    rows.extend(chain_rows(&l.chain, indent));
}

/// The rows of a value's chain ([`write_chain`], `why`).
fn chain_rows(chain: &[tree::Step], indent: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for step in chain {
        let word = if step.lost { "over" } else { "=" };
        let rank = step
            .rank
            .as_ref()
            .map(|r| format!(" @{r}"))
            .unwrap_or_default();
        let left = format!("{indent}{word} {}{rank}", elide_literals(&step.expr));
        let mut right = Vec::new();
        if !step.with.is_empty() {
            right.push(
                format!("{}  with {}", step.at, step.with.join(", "))
                    .trim()
                    .to_string(),
            );
        }
        if !step.at.is_empty() {
            right.push(step.at.clone());
        }
        rows.push(Row::plain(left).with(right));
    }
    rows
}

/// One value `why` prints ([`chains_text`]).
pub struct ChainItem {
    /// `path = value`; before a fold's value, `path = `.
    pub head: String,
    /// The value as its head says it.
    pub shown: String,
    pub chain: Vec<tree::Step>,
    /// A fold (R-124): the value one contribution wrote, laid out after
    /// `head`.
    pub value: Option<crate::fmt::value::Tree>,
}

/// Values and their chains as `why` prints them (R-122): each `head`
/// (`path = value`) at `indent`, its steps under it, in one layout. A
/// chain that only says the value (`shown`) again is left out. A long
/// string in a head is elided, or with `whole` (`why -vv`, R-176) printed
/// whole, a line break in it as one (`whole_lines`).
pub fn chains_text(items: &[ChainItem], indent: &str, style: Style, whole: bool) -> String {
    let mut rows = Vec::new();
    for it in items {
        // A literal: its place on its (first) line.
        let literal = match it.chain.as_slice() {
            [only] if !only.lost && only.with.is_empty() && only.expr == it.shown => {
                Some(only.at.clone())
            }
            _ => None,
        };
        if let Some(t) = &it.value {
            let width = WIDTH.saturating_sub(indent.chars().count());
            for (i, text) in crate::fmt::value::layout(&it.head, t, width)
                .into_iter()
                .enumerate()
            {
                let row = Row::plain(format!("{indent}{text}"));
                rows.push(match (i, &literal) {
                    (0, Some(at)) => row.with(vec![at.clone()]),
                    _ => row,
                });
            }
            if literal.is_none() {
                rows.extend(chain_rows(&it.chain, &format!("{indent}  ")));
            }
            continue;
        }
        let (head, more) = match whole {
            true => {
                let mut lines = whole_lines(&it.head).into_iter();
                (lines.next().unwrap_or_default(), lines.collect())
            }
            false => (elide_literals(&it.head), Vec::new()),
        };
        // A string's further lines are its text, not indented; the site
        // follows its last.
        let mut lines = vec![Row::plain(format!("{indent}{head}"))];
        lines.extend(more.into_iter().map(Row::plain));
        if let Some(at) = &literal
            && let Some(last) = lines.pop()
        {
            lines.push(last.with(vec![at.clone()]));
        }
        rows.extend(lines);
        if literal.is_none() {
            rows.extend(chain_rows(&it.chain, &format!("{indent}  ")));
        }
    }
    layout(&rows, style)
}

/// `e` whole, each `\n` in a string literal a line break (R-61's form of
/// a string that spans lines): its lines.
fn whole_lines(e: &str) -> Vec<String> {
    let mut out = String::new();
    let mut quoted = false;
    let mut cs = e.chars();
    while let Some(c) = cs.next() {
        match c {
            '"' => {
                quoted = !quoted;
                out.push(c);
            }
            '\\' if quoted => match cs.next() {
                Some('n') => out.push('\n'),
                Some(x) => {
                    out.push(c);
                    out.push(x);
                }
                None => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out.split('\n').map(str::to_string).collect()
}

/// `e` with each string literal past [`LONG`] characters elided.
fn elide_literals(e: &str) -> String {
    let mut out = String::new();
    let mut rest = e;
    while let Some(start) = rest.find('"') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let mut end = None;
        let mut escaped = false;
        for (i, ch) in tail.char_indices() {
            match ch {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    end = Some(i);
                    break;
                }
                _ => escaped = false,
            }
        }
        let Some(end) = end else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push('"');
        out.push_str(&elide(&tail[..end]));
        out.push('"');
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}
