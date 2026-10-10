//! One change in the plan's text: its header, each attribute line with its
//! value, and the site column beside it (R-111, after R-149): where the change is
//! derived, what wrote a value, what it beat, why a delete is planned, a change's
//! bindings as the default level says them.

use super::chains::write_chain;
use super::deformation::{Deformation, Kept, Line, Op};
use super::fold::scalar;
use super::labels::{address, marker_of, reference};
use super::layout::{Row, WIDTH};
use super::mask::{Shown, elide, masked_text};
use super::style::{Paint, Style};
use super::tree::Site;
use super::{FORGOTTEN, KEPT, Report, Why};
use crate::address::Address;
use crate::engine::EvalResult;
use crate::provider::ActionKind;
use crate::query::Redactor;
use std::collections::BTreeSet;

impl Report {
    /// One change (R-111): its marker and address with where it is
    /// derived, its attributes each with where its value was written when
    /// that is outside its block, at `-vv` what it rests on, and why it
    /// changed since the last apply.
    pub(super) fn write_change(
        &self,
        rows: &mut Vec<Row>,
        d: &Deformation,
        indent: &str,
        style: Style,
    ) {
        let note = match d.kind {
            ActionKind::Drift => {
                "  (drift: a fresh null where the world has a value; its identity is stale)"
            }
            ActionKind::DeleteDeposed => "  (deposed)",
            ActionKind::Replace { create_first: true } => "  replace, create first",
            ActionKind::Forget => FORGOTTEN,
            _ => "",
        };
        // The replacement's remote name, where dform gives it (R-189).
        let note = match &d.renamed {
            Some((was, now)) => format!("{note} ({was} → {now})"),
            None => note.to_string(),
        };
        let addr = address(&d.addr);
        let plain = format!("{indent}{} {addr}{note}", marker_of(&d.kind));
        let painted = format!(
            "{indent}{} {}{note}",
            style.marker(&d.kind),
            style.address(&d.kind, &addr)
        );
        let right = self.change_column(d, &addr);
        rows.push(Row::new(&plain, painted).with(right));
        self.write_attrs(rows, d, &format!("{indent}    "), style);
    }
    /// A change line's right column: where it is derived and, for a
    /// replace, what forces it; a delete's reason; a create's bindings at
    /// the default level; its custody.
    fn change_column(&self, d: &Deformation, addr: &str) -> Vec<String> {
        let at = d.site.as_ref().map(|s| place_text(s, self.why));
        let right: Vec<String> = match (&d.kind, at) {
            (ActionKind::Replace { .. }, at) => {
                let forces: Vec<String> = d
                    .forces
                    .iter()
                    .map(|p| format!("{p} forces replace"))
                    .collect();
                let both = at
                    .iter()
                    .flat_map(|at| forces.iter().map(move |f| format!("{at}  {f}")));
                both.chain(forces.iter().cloned()).collect()
            }
            (ActionKind::Delete, _) if !self.removing => gone_column(d, self.why),
            (ActionKind::Delete | ActionKind::DeleteDeposed, _) => vec![],
            // A create: the bindings of the clause that made this one,
            // those its address does not show (After R-149 amendment 5).
            (ActionKind::Create | ActionKind::Adopt, Some(at)) if self.why == Why::Line => {
                let s = d.site.as_ref().expect("a site");
                // A value the body prints (an attribute, a document by
                // its row) is said there.
                let shown: BTreeSet<String> = values(d, |l| &l.after)
                    .into_iter()
                    .map(|(_, v)| v.trim_matches('"').to_string())
                    .chain(
                        d.folded
                            .iter()
                            .chain(&d.lines)
                            .filter_map(|l| l.row.clone()),
                    )
                    .collect();
                // A value the clause reads (`agents` of `i in 0..agents`)
                // is no binding of this row: the clause's are.
                let with: Vec<String> = s
                    .with
                    .iter()
                    .filter(|b| !s.reads.contains(b))
                    .filter(|b| {
                        !b.split_once(" = ")
                            .is_some_and(|(_, v)| shown.contains(v.trim_matches('"')))
                    })
                    .cloned()
                    .collect();
                match terse(&with, addr) {
                    Some(w) => vec![format!("{at}  {w}"), at],
                    None => vec![at],
                }
            }
            (_, Some(at)) => match d.site.as_ref() {
                // A line too long for its bindings keeps its place.
                Some(s) if at != s.at && !s.at.is_empty() => vec![at, s.at.clone()],
                _ => vec![at],
            },
            (_, None) => vec![],
        };
        match &d.custody {
            Some(c) if right.is_empty() => vec![c.clone()],
            Some(c) => right.into_iter().map(|r| format!("{r}  {c}")).collect(),
            None => right,
        }
    }

    /// A change's attribute lines (the forced first, at most 40), each
    /// with its site and at `-vv` its chain; the values kept; why it
    /// changed since the last apply.
    fn write_attrs(&self, rows: &mut Vec<Row>, d: &Deformation, inner: &str, style: Style) {
        // Keep plan output readable.
        let max = 40usize;
        let lines = match d.folded.is_empty() {
            true => &d.lines,
            false => &d.folded,
        };
        // A replace: the attribute that forces it first (After R-149
        // amendment 5).
        let forced = |l: &Line| {
            d.forces.iter().any(|p| {
                l.path == *p
                    || l.path
                        .strip_prefix(p.as_str())
                        .is_some_and(|r| r.starts_with('.') || r.starts_with('['))
            })
        };
        let mut lines: Vec<&Line> = lines.iter().collect();
        lines.sort_by_key(|l| !forced(l));
        for (i, l) in lines.iter().copied().enumerate() {
            if i == max {
                rows.push(Row::plain(format!(
                    "{inner}... ({} more changes)",
                    lines.len() - max
                )));
                break;
            }
            let right = match &l.site {
                None => vec![],
                Some(s) => attr_text(d, l, s, self.why),
            };
            write_line(rows, &d.kind, l, inner, style, self.why, right);
            if self.why == Why::Full {
                write_chain(rows, l, &format!("{inner}  "), self.why);
            }
        }
        for k in &d.kept {
            write_kept(rows, k, inner, style, self.why);
        }
        // A delete's reason is in its change line's site column (After
        // R-149), a destroy's none: no line under its attributes.
        if let Some(b) = d.because.as_ref().filter(|_| !is_delete(&d.kind)) {
            let plain = format!("{inner}because {b}");
            let painted = format!("{inner}{} {b}", style.paint(Paint::Because, "because"));
            rows.push(Row::new(&plain, painted));
        }
    }
}

pub(super) fn write_line(
    rows: &mut Vec<Row>,
    kind: &ActionKind,
    l: &Line,
    indent: &str,
    style: Style,
    why: Why,
    right: Vec<String>,
) {
    let mut push = |plain: String, painted: String, right: Vec<String>| {
        rows.push(Row::new(&plain, painted).with(right))
    };
    let plain = |v: &Shown| v.said(why);
    let painted = |v: &Shown| style.said(v, why);
    match l.op {
        Op::Add | Op::Remove => write_element(rows, l, indent, style, why, right),
        // A document value (R-131): its row; a value body's, the
        // resource's (`= vendor/crds.yml:412  (24.0 KB)`).
        Op::Leaf if let Some(row) = &l.row => {
            let text = match l.path.is_empty() {
                true => format!("{indent}= {row}"),
                false => format!("{indent}{} = {row}", l.path),
            };
            push(text.clone(), text, right);
        }
        Op::Leaf if l.value.is_some() => write_folded(rows, l, indent, style, right),
        // An element named by itself says no value (R-158).
        Op::Leaf
            if matches!(kind, ActionKind::Create | ActionKind::Adopt)
                && scalar(&l.after)
                && l.path.ends_with(&format!("[{}]", plain(&l.after))) =>
        {
            let text = format!("{indent}{}", l.path);
            push(text.clone(), text, right);
        }
        Op::Leaf => match kind {
            ActionKind::Create | ActionKind::Adopt => push(
                format!("{indent}{} = {}", l.path, plain(&l.after)),
                format!("{indent}{} = {}", l.path, painted(&l.after)),
                right,
            ),
            ActionKind::Delete | ActionKind::DeleteDeposed => push(
                format!("{indent}{} = {}", l.path, plain(&l.before)),
                format!("{indent}{} = {}", l.path, painted(&l.before)),
                right,
            ),
            ActionKind::Update
            | ActionKind::Drift
            | ActionKind::Pending
            | ActionKind::Replace { .. } => push(
                format!(
                    "{indent}{} = {} → {}",
                    l.path,
                    plain(&l.before),
                    plain(&l.after)
                ),
                format!(
                    "{indent}{} = {} → {}",
                    l.path,
                    painted(&l.before),
                    painted(&l.after)
                ),
                right,
            ),
            ActionKind::Noop | ActionKind::Forget => {}
        },
    }
}

/// An element added or removed (`+`/`-`), by itself when it is a scalar
/// of a keyless set, else its path and then each of its leaves.
fn write_element(
    rows: &mut Vec<Row>,
    l: &Line,
    indent: &str,
    style: Style,
    why: Why,
    right: Vec<String>,
) {
    let mut push = |plain: String, painted: String, right: Vec<String>| {
        rows.push(Row::new(&plain, painted).with(right))
    };
    let plain = |v: &Shown| v.said(why);
    let painted = |v: &Shown| style.said(v, why);
    let (sign, paint, v) = if l.op == Op::Add {
        ("+", Paint::Create, &l.after)
    } else {
        ("-", Paint::Delete, &l.before)
    };
    let painted_sign = style.paint(paint, sign);
    // A scalar element of a keyless set is named by itself
    // (R-158): `- policies[app_policy]`.
    if l.leaves.is_empty()
        && let Some(list) = l.path.strip_suffix("[]")
    {
        let (plain, painted) = (plain(v), painted(v));
        push(
            format!("{indent}{sign} {list}[{plain}]"),
            format!("{indent}{painted_sign} {list}[{painted}]"),
            right,
        );
        return;
    }
    if l.leaves.is_empty() {
        push(
            format!("{indent}{sign} {} = {}", l.path, plain(v)),
            format!("{indent}{painted_sign} {} = {}", l.path, painted(v)),
            right,
        );
        return;
    }
    push(
        format!("{indent}{sign} {}", l.path),
        format!("{indent}{painted_sign} {}", l.path),
        right,
    );
    let inner = if l.op == Op::Add {
        ActionKind::Create
    } else {
        ActionKind::Delete
    };
    for x in &l.leaves {
        write_line(
            rows,
            &inner,
            x,
            &format!("{indent}    "),
            style,
            why,
            vec![],
        );
    }
}

/// A folded value (R-124): laid out as the formatter writes it, the right
/// column on its first row.
fn write_folded(rows: &mut Vec<Row>, l: &Line, indent: &str, style: Style, right: Vec<String>) {
    let mut push = |plain: String, painted: String, right: Vec<String>| {
        rows.push(Row::new(&plain, painted).with(right))
    };
    let head = format!("{} = ", l.path);
    let width = WIDTH.saturating_sub(indent.chars().count());
    let tree = l.value.as_ref().expect("a fold has its value");
    for (i, text) in crate::fmt::value::layout(&head, tree, width)
        .into_iter()
        .enumerate()
    {
        let row = format!("{indent}{text}");
        let painted = style.notes_in(&row);
        match i {
            0 => push(row, painted, right.clone()),
            _ => push(row, painted, vec![]),
        }
    }
}

/// A value given at creation only that differs from the object's (R-198):
/// `user_data differs (bootstrap): kept`, then why where dform can tell,
/// a note (`  (the key was replaced)`, R-218); from `-v` the two values,
/// the object's first.
pub(super) fn write_kept(rows: &mut Vec<Row>, k: &Kept, indent: &str, style: Style, why: Why) {
    let l = &k.line;
    let (plain, painted) = match why >= Why::How {
        true => (
            format!(
                "{indent}{} = {} → {}  {KEPT}",
                l.path,
                l.before.said(why),
                l.after.said(why)
            ),
            format!(
                "{indent}{} = {} → {}  {}",
                l.path,
                style.said(&l.before, why),
                style.said(&l.after, why),
                style.note(KEPT)
            ),
        ),
        false => (
            format!("{indent}{} differs {KEPT}", l.path),
            format!("{indent}{} differs {}", l.path, style.note(KEPT)),
        ),
    };
    let (plain, painted) = match &k.note {
        Some(n) => (
            format!("{plain}  {n}"),
            format!("{painted}  {}", style.note(n)),
        ),
        None => (plain, painted),
    };
    rows.push(Row::new(&plain, painted));
}

/// A row's right column, the longest that fits first: the place, the
/// condition, and (at `full`) the reason; the condition alone last.
pub(super) fn both(full: bool, at: String, cond: String, reason: &str) -> Vec<String> {
    let mut out = Vec::new();
    for cond in [full.then(|| format!("{cond}  ({reason})")), Some(cond)]
        .into_iter()
        .flatten()
    {
        if !at.is_empty() {
            out.push(format!("{at}  {cond}"));
        }
        out.push(cond);
    }
    out
}

/// A change's bindings as the default level says them (After R-149
/// amendment 5): only those whose value its address does not show as a
/// whole segment (`k3s.agent-3` shows `n = 3`; `private-us-east-1b` shows
/// `zone = "us-east-1b"`, not `n = 1`), each value elided as any long one,
/// at most two and then `…`: `with zone = "us-test-1a", n = 3, …`.
pub(crate) fn terse(with: &[String], addr: &str) -> Option<String> {
    let shown: Vec<String> = with
        .iter()
        .filter(|b| match b.split_once(" = ") {
            Some((_, v)) => !shows(addr, v.trim_matches('"')),
            None => true,
        })
        .map(|b| match b.split_once(" = ") {
            Some((k, v)) => format!("{k} = {}", elide(v)),
            None => b.clone(),
        })
        .collect();
    let mut out = shown.iter().take(2).cloned().collect::<Vec<_>>().join(", ");
    if shown.len() > 2 {
        out.push_str(", …");
    }
    (!out.is_empty()).then(|| format!("with {out}"))
}

/// Whether `addr` shows `v` whole: `v` in it with no letter or digit
/// either side, so a segment (`3` of `agent-3`, `us-east-1b` of
/// `private-us-east-1b`) and not a part of one (`1` of `1b`).
fn shows(addr: &str, v: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    !v.is_empty()
        && addr.match_indices(v).any(|(i, _)| {
            !word(addr[..i].chars().next_back()) && !word(addr[i + v.len()..].chars().next())
        })
}

/// A delete, of the object or of a deposed one.
fn is_delete(k: &ActionKind) -> bool {
    matches!(k, ActionKind::Delete | ActionKind::DeleteDeposed)
}

/// The leaves of `d` with the values `side` gives, as the plan prints
/// them: what a rename guess compares.
pub(super) fn values(
    d: &Deformation,
    side: impl Fn(&Line) -> &Shown,
) -> BTreeSet<(String, String)> {
    fn walk(ls: &[Line], side: &dyn Fn(&Line) -> &Shown, out: &mut BTreeSet<(String, String)>) {
        for l in ls {
            match l.leaves.is_empty() {
                true => {
                    out.insert((l.path.clone(), side(l).said(Why::Line)));
                }
                false => walk(&l.leaves, side, out),
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(&d.lines, &side, &mut out);
    out
}

/// Why a plan deletes `d`, on one line (After R-149, R-150): a create of
/// its type in the same plan with the same values is a rename guess
/// (`renamed?  T b is created with the same values`, `state mv` the fix);
/// a copy whose `use` the program no longer has (`use synapse removed`);
/// else why the program no longer derives it ([`crate::why::not::gone`]),
/// `not in the program` with where the last apply derived it when that is
/// known.
pub(super) fn gone(
    d: &Deformation,
    created: &[(Address, BTreeSet<(String, String)>)],
    instances: &crate::zset::Instances,
    live: &crate::zset::Instances,
    res: &EvalResult,
    r: &Redactor,
) -> Option<(Option<String>, String)> {
    let mine = values(d, |l| &l.before);
    if !mine.is_empty()
        && let Some((a, _)) = created
            .iter()
            .find(|(a, vs)| a.typ == d.addr.typ && *vs == mine)
    {
        return Some((
            None,
            format!(
                "renamed?  {} is created with the same values",
                reference(a, "")
            ),
        ));
    }
    let wanted = live.enclosing(&d.addr);
    if let Some(copy) = instances
        .enclosing(&d.addr)
        .into_iter()
        .find(|c| !wanted.contains(c))
    {
        return Some((None, format!("use {} removed", copy.name)));
    }
    crate::why::not::gone(&d.addr.typ, &d.addr.name, res, r)
}

/// A delete's site column (After R-149): where the last apply derived
/// it (else where the rule that would is), then why it is gone, the
/// leaf the last apply rests on that is false now when the plan knows it
/// (`because`), else the program's why-not; `not in the program  (was
/// SITE)`; a rename guess alone. The reason alone when that is too
/// wide, else the site;
/// from `-v`, the site with the bindings the last apply derived it with.
fn gone_column(d: &Deformation, how: Why) -> Vec<String> {
    let Some((rule_at, why)) = &d.gone else {
        return d
            .site
            .iter()
            .map(|s| s.at.clone())
            .filter(|a| !a.is_empty())
            .collect();
    };
    // From `-v`, the bindings it was derived with.
    let at = d
        .site
        .as_ref()
        .map(|s| place_text(s, how))
        .filter(|a| !a.is_empty())
        .or_else(|| rule_at.clone());
    if why.starts_with("renamed?") {
        return vec![why.clone()];
    }
    let why = d.because.clone().unwrap_or_else(|| why.clone());
    match at {
        Some(at) if why == crate::why::not::NOT_IN_PROGRAM => {
            vec![format!("{why}  (was {at})"), why]
        }
        Some(at) => vec![format!("{at}  {why}"), why, at],
        None => vec![why],
    }
}

/// The site column of attribute line `l` of change `d`, written at `s`,
/// at level `why` (R-111). By default a value written in its own block
/// says nothing, any other its place. From `-v`, a create's value
/// written in its own block is the entry's expression when it reads
/// something the value does not show; anything else is the statement
/// that wrote it; either with the writes it won over.
fn attr_text(d: &Deformation, l: &Line, s: &Site, why: Why) -> Vec<String> {
    if matches!(d.kind, ActionKind::Delete | ActionKind::DeleteDeposed) {
        return vec![];
    }
    let texts = attr_texts(d, l, s, why);
    // An expression written into a secret says no literal (R-124
    // amendment 2).
    match (&l.after, &l.before) {
        (Shown::Sensitive(_), _) | (_, Shown::Sensitive(_)) => {
            texts.iter().map(|t| masked_text(t)).collect()
        }
        _ => texts,
    }
}

fn attr_texts(d: &Deformation, l: &Line, s: &Site, why: Why) -> Vec<String> {
    let own = d.site.as_ref().is_some_and(|h| match (&h.stmt, &s.stmt) {
        (Some((hf, first)), Some((sf, line))) => {
            hf == sf && (first == line || (first..=&h.last).contains(&line))
        }
        _ => false,
    });
    if why == Why::Line {
        return match own {
            true => vec![],
            false => vec![place_text(s, why)],
        };
    }
    if own && matches!(d.kind, ActionKind::Create | ActionKind::Adopt) {
        let after = l.after.said(why);
        let rhs = s
            .entry
            .as_deref()
            .and_then(|e| e.split_once(" = "))
            .map(|(_, rhs)| rhs.to_string())
            // A fold is the value the entry wrote, laid out.
            .filter(|_| l.value.is_none())
            .filter(|rhs| {
                // A variable is the entry's binding, on the line above; a
                // literal is the value itself; a secret says so already.
                !rhs.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !rhs.starts_with("(sensitive")
                    && reads(rhs)
                    && !after.contains(rhs.as_str())
            });
        let beat = beat_text(s, d);
        return match rhs {
            Some(rhs) if !beat.is_empty() => vec![format!("{rhs}{beat}"), rhs],
            Some(rhs) => vec![rhs],
            None if !beat.is_empty() => vec![beat.trim_start().to_string()],
            None => vec![],
        };
    }
    written_text(s, d)
}

/// Whether expression `e` reads anything: a name that is not an object's
/// key (a variable, a function), or an interpolation. `{ team: "a" }`
/// reads nothing; `db.name`, `io.read("f.json")` and `"shop-${env}"` do.
pub(super) fn reads(e: &str) -> bool {
    let mut cs = e.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '"' {
            let mut esc = false;
            let mut prev = ' ';
            for c in cs.by_ref() {
                if prev == '$' && c == '{' && !esc {
                    return true;
                }
                if c == '"' && !esc {
                    break;
                }
                esc = c == '\\' && !esc;
                prev = c;
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut word = String::from(c);
            while let Some(&n) = cs.peek()
                && (n.is_alphanumeric() || "_.".contains(n))
            {
                word.push(n);
                cs.next();
            }
            while cs.peek().is_some_and(|n| n.is_whitespace()) {
                cs.next();
            }
            let key = cs.peek() == Some(&':');
            if !key && !matches!(word.as_str(), "true" | "false" | "null") {
                return true;
            }
        }
    }
    false
}

/// Where a change is derived, as its line's site column says it at `why`
/// (R-111): `FILE:LINE` (a value given on the command line, the flag);
/// from `-v`, the statement's bindings after it, `with z = "a"`.
fn place_text(s: &Site, why: Why) -> String {
    let mut out = match s.at.is_empty() {
        true => s.statement.clone(),
        false => s.at.clone(),
    };
    if why >= Why::How && !s.with.is_empty() {
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(&format!("with {}", s.with.join(", ")));
    }
    out
}

/// The writes a winning value of change `d` overrode, from `-v` (R-111):
/// `  @normal over k3s.df:9 @default`; not one in `d`'s own block (a
/// default the compiler writes there).
fn beat_text(s: &Site, d: &Deformation) -> String {
    let own = |at: &str| {
        let (Some(h), Some((file, line))) = (&d.site, at.rsplit_once(':')) else {
            return false;
        };
        let Some((hf, first)) = h.at.rsplit_once(':') else {
            return false;
        };
        let (Ok(line), Ok(first)) = (line.parse::<usize>(), first.parse::<usize>()) else {
            return false;
        };
        hf == file && (first..=h.last.max(first)).contains(&line)
    };
    match (&s.beat, &s.beat_at) {
        (Some(_), Some(at)) if own(at) => String::new(),
        (Some(b), Some(at)) => {
            let rank = s.rank.as_deref().unwrap_or("normal");
            format!("  @{rank} over {at} @{b}")
        }
        (Some(b), None) => format!("  over @{b}"),
        _ => String::new(),
    }
}

/// What wrote an attribute's value at `-v`: `STATEMENT   FILE:LINE`, then
/// `FILE:LINE` alone (a constant: its entry when it reads something, else
/// its place); the writes it won over after either.
fn written_text(s: &Site, d: &Deformation) -> Vec<String> {
    let beat = beat_text(s, d);
    if s.at.is_empty() {
        return vec![format!("{}{beat}", s.statement)];
    }
    if s.stated {
        let entry = s
            .entry
            .iter()
            .filter(|e| e.split_once(" = ").is_some_and(|(_, rhs)| reads(rhs)))
            .map(|e| format!("{e}   {}{beat}", s.at));
        return entry.chain([format!("{}{beat}", s.at)]).collect();
    }
    let mut out = vec![format!("{}   {}{beat}", s.statement, s.at)];
    out.extend(s.entry.iter().map(|e| format!("{e}   {}{beat}", s.at)));
    out.push(format!("{}{beat}", s.at));
    out
}
