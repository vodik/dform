//! Apply's progress (R-127, R-206): the tick's block, its lines the
//! plan's (one printer, R-200: a copy's and a used module's changes
//! nested under its header as the plan nests them), filling in. Each
//! change is its mark and full name, then where it is: what it reads that
//! the tick makes first, or its call's word and time, `making 0.8s` dim
//! while it runs (the provider's own status word verbatim when it streams
//! one, R-130), `made 1.1s` set once it answered. The tick's header
//! carries a fill bar (calls answered over the tick's calls, never a
//! spinner) and the time it has run, `done 4.2s` once it ends. A failed
//! change's mark is `!`; its error is said once, in full, below the block
//! ([`Block::failures`]), in R-109's shape. Colour is on the marks only.
//! The driver (the `dform` binary's `progress`) prints it: redrawn in
//! place on a terminal, a line per change of state otherwise.

use super::{
    Address, Failure, Node, Paint, Row, Style, address, attribute_label, kind_paint, layout,
    marker_of,
};
use crate::provider::{Action, ActionKind, NULL_KEY, marker};
use serde_json::Value as Json;
use std::time::{Duration, Instant};

/// Where a change of the tick is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Not started: on what it reads that the tick makes first, if the
    /// plan names it.
    Waiting(Option<String>),
    Running,
    Done,
    /// Its call failed: said below the block.
    Failed,
    /// Never started: the apply was interrupted first (a running one is
    /// awaited, and ends `Done` or `Failed`).
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub addr: Address,
    kind: ActionKind,
    mark: &'static str,
    printed: String,
    pub state: State,
    started: Option<Instant>,
    took: Option<Duration>,
    /// The provider's status word, as it says it.
    pub status: Option<String>,
}

/// A line of the block's tree: a copy's or a module's header, or a
/// change (an index into [`Block::entries`]).
#[derive(Debug, Clone)]
enum Item {
    Header { kind: ActionKind, printed: String },
    Change(usize),
}

/// One tick's block.
#[derive(Debug, Clone)]
pub struct Block {
    pub tick: usize,
    pub entries: Vec<Entry>,
    /// The lines under the header, each at its depth, as the plan nests
    /// them.
    tree: Vec<(usize, Item)>,
    started: Instant,
    /// How long the tick ran, once it ended.
    ended: Option<Duration>,
    /// Every failure's full error, in the order they came.
    pub errors: Vec<(Address, Failure)>,
    /// Where each change is derived (`k3s.df:66`), for its failure.
    pub sites: std::collections::BTreeMap<Address, String>,
}

/// A duration as progress says it: `0.8s`, `42s`, `1m12s`, `1h3m`.
pub fn took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..10 => format!("{:.1}s", d.as_secs_f64()),
        10..60 => format!("{s}s"),
        60..3600 => format!("{}m{}s", s / 60, s % 60),
        _ => format!("{}h{}m", s / 3600, s % 3600 / 60),
    }
}

/// A tick's title, `tick 1  3 changes`: the block's header, and the
/// question a later tick asks on it (`tick 2  1 change   apply? [y/N]`).
pub fn title(tick: usize, n: usize) -> String {
    format!("tick {tick}  {n} change{}", if n == 1 { "" } else { "s" })
}

/// Whether action `a` is a call the tick makes: a line of its block.
pub fn is_call(a: &Action) -> bool {
    !matches!(a.kind, ActionKind::Noop | ActionKind::Pending)
}

/// The cells of the header's fill bar.
const BAR: usize = 12;

/// `━━━━━━━━░░░░`: `done` of `n` filled.
fn bar(done: usize, n: usize) -> String {
    let full = match n {
        0 => BAR,
        _ => (BAR * done + n / 2) / n,
    };
    format!("{}{}", "━".repeat(full), "░".repeat(BAR - full))
}

/// What a call of kind `k` says while it runs, and once it answered:
/// `making`, `made`.
fn words(k: &ActionKind) -> (&'static str, &'static str) {
    match k {
        ActionKind::Create => ("making", "made"),
        ActionKind::Adopt => ("adopting", "adopted"),
        ActionKind::Replace { .. } => ("replacing", "replaced"),
        ActionKind::Delete | ActionKind::DeleteDeposed => ("deleting", "deleted"),
        // A forget drops the object from state and keeps it in the world.
        ActionKind::Forget => ("keeping", "kept"),
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending | ActionKind::Noop => {
            ("updating", "updated")
        }
    }
}

impl Block {
    /// The tick's changes, in plan order, each waiting.
    pub fn new(tick: usize, actions: &[&Action]) -> Block {
        let entries: Vec<Entry> = actions
            .iter()
            .filter(|a| is_call(a))
            .map(|a| Entry {
                addr: a.addr.clone(),
                kind: a.kind.clone(),
                mark: marker_of(&a.kind),
                printed: address(&a.addr),
                state: State::Waiting(reads(a)),
                started: None,
                took: None,
                status: None,
            })
            .collect();
        Block {
            tick,
            tree: (0..entries.len()).map(|i| (0, Item::Change(i))).collect(),
            entries,
            started: Instant::now(),
            ended: None,
            errors: Vec::new(),
            sites: Default::default(),
        }
    }

    /// The changes in the plan's tree ([`super::Report::outline`]): in
    /// its order, nested under the headers it has; a change it does not
    /// list after them, a header with no change of the tick under it
    /// left out.
    pub fn nested(mut self, outline: &[(usize, Node)]) -> Block {
        let mut tree: Vec<(usize, Item)> = Vec::new();
        let mut placed = vec![false; self.entries.len()];
        for (depth, node) in outline {
            match node {
                Node::Header { addr, kind } => tree.push((
                    *depth,
                    Item::Header {
                        kind: kind.clone(),
                        printed: address(addr),
                    },
                )),
                Node::Change(d) => {
                    let i = self.entries.iter().position(|e| e.addr == d.addr);
                    if let Some(i) = i.filter(|i| !placed[*i]) {
                        placed[i] = true;
                        tree.push((*depth, Item::Change(i)));
                    }
                }
            }
        }
        tree.extend(
            (0..self.entries.len())
                .filter(|i| !placed[*i])
                .map(|i| (0, Item::Change(i))),
        );
        // A header stays when a change follows it deeper.
        let keep: Vec<bool> = (0..tree.len())
            .map(|k| match tree[k].1 {
                Item::Change(_) => true,
                Item::Header { .. } => tree[k + 1..]
                    .iter()
                    .take_while(|(d, _)| *d > tree[k].0)
                    .any(|(_, i)| matches!(i, Item::Change(_))),
            })
            .collect();
        let mut keep = keep.into_iter();
        tree.retain(|_| keep.next().unwrap_or(true));
        self.tree = tree;
        self
    }

    fn entry(&mut self, addr: &Address) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.addr == *addr)
    }

    /// Its Apply call was submitted.
    pub fn start(&mut self, addr: &Address) {
        if let Some(e) = self.entry(addr) {
            e.state = State::Running;
            e.started = Some(Instant::now());
        }
    }

    /// Its Apply call answered.
    pub fn done(&mut self, addr: &Address) {
        if let Some(e) = self.entry(addr) {
            e.state = State::Done;
            e.took = e.started.map(|s| s.elapsed());
        }
    }

    /// Its Apply call failed with `error`: its line keeps its mark and
    /// time; the error is said below the block, at where the change is
    /// derived unless it says where.
    pub fn fail(&mut self, addr: &Address, error: Failure) {
        if let Some(e) = self.entry(addr) {
            e.state = State::Failed;
            e.took = e.started.map(|s| s.elapsed());
        }
        let error = error.at(self.sites.get(addr).cloned());
        self.errors.push((addr.clone(), error));
    }

    /// What never started is interrupted: the apply was asked to stop.
    pub fn interrupt(&mut self) {
        for e in &mut self.entries {
            if matches!(e.state, State::Waiting(_)) {
                e.state = State::Interrupted;
            }
        }
    }

    /// The tick ended: its time stops.
    pub fn end(&mut self) {
        self.ended.get_or_insert_with(|| self.started.elapsed());
    }

    /// The failures, each in full in R-109's shape, `! ` before its first
    /// line: what the block says below it once the tick ends.
    pub fn failures(&self, style: Style) -> Vec<String> {
        let mut out = Vec::new();
        for (_, f) in &self.errors {
            for (i, l) in f.lines("! ").into_iter().enumerate() {
                out.push(match i {
                    0 => format!("{} {}", style.paint(Paint::Error, "!"), &l[2..]),
                    _ => l,
                });
            }
        }
        out
    }

    /// Its title, `tick 1  3 changes`.
    pub fn title(&self) -> String {
        title(self.tick, self.entries.len())
    }

    /// How the tick ended, `done`, `failed`, `interrupted`; `None` while
    /// it runs.
    fn outcome(&self) -> Option<&'static str> {
        self.ended?;
        let any = |s: State| self.entries.iter().any(|e| e.state == s);
        Some(match (any(State::Failed), any(State::Interrupted)) {
            (true, _) => "failed",
            (_, true) => "interrupted",
            _ => "done",
        })
    }

    /// The tick's time: as of now, or what it ran.
    fn elapsed(&self) -> Duration {
        self.ended.unwrap_or_else(|| self.started.elapsed())
    }

    /// The header's right column on a terminal: the fill bar, then `2 of
    /// 3  3.1s` while the tick runs, `done 4.2s` once it ended.
    pub fn progress(&self) -> String {
        let n = self.entries.len();
        let answered = self
            .entries
            .iter()
            .filter(|e| matches!(e.state, State::Done | State::Failed))
            .count();
        let time = took(self.elapsed());
        match self.outcome() {
            Some(word) => format!("{}  {word} {time}", bar(answered, n)),
            None => format!("{}  {answered} of {n}  {time}", bar(answered, n)),
        }
    }

    /// An entry's mark painted as the plan paints it (hints only: the
    /// mark, never the line), `!` red once its call failed.
    fn mark(e: &Entry, style: Style) -> (&'static str, String) {
        match (&e.state, kind_paint(&e.kind)) {
            (State::Failed, _) => ("!", style.paint(Paint::Error, "!")),
            (_, Some(p)) => (e.mark, style.paint(p, e.mark)),
            (_, None) => (e.mark, e.mark.to_string()),
        }
    }

    /// Change `i`'s right column as of now, and whether it is set (its
    /// call over) rather than dim.
    fn state(&self, i: usize) -> (String, bool) {
        let e = &self.entries[i];
        let (running, ran) = words(&e.kind);
        let time = || e.took.or_else(|| e.started.map(|s| s.elapsed())).map(took);
        match &e.state {
            State::Waiting(Some(on)) => (format!("waits on {on}"), false),
            State::Waiting(None) => (String::new(), false),
            State::Running => {
                let word = e.status.as_deref().unwrap_or(running);
                (format!("{word} {}", time().unwrap_or_default()), false)
            }
            State::Done => (format!("{ran} {}", time().unwrap_or_default()), true),
            State::Failed => (format!("failed {}", time().unwrap_or_default()), true),
            State::Interrupted => match time() {
                Some(t) => (format!("interrupted {t}"), true),
                None => ("interrupted".to_string(), true),
            },
        }
    }

    /// The block's rows: its header (with `bar`, the fill bar and the
    /// time), then each line of its tree.
    fn rows(&self, style: Style, bar: bool) -> Vec<Row> {
        let head = self.title();
        let mut header = Row::new(&head, style.paint(Paint::Bold, &head));
        if bar {
            header = header.with(vec![self.progress()]).set().aligned();
        }
        let mut rows = vec![header];
        for (depth, item) in &self.tree {
            let indent = "  ".repeat(depth + 1);
            rows.push(match item {
                Item::Header { kind, printed } => {
                    let mark = marker_of(kind);
                    let painted = match kind_paint(kind) {
                        Some(p) => style.paint(p, mark),
                        None => mark.to_string(),
                    };
                    Row::new(
                        &format!("{indent}{mark} {printed}"),
                        format!("{indent}{painted} {printed}"),
                    )
                }
                Item::Change(i) => {
                    let e = &self.entries[*i];
                    let (plain, painted) = Block::mark(e, style);
                    let row = Row::new(
                        &format!("{indent}{plain} {}", e.printed),
                        format!("{indent}{painted} {}", e.printed),
                    )
                    .aligned();
                    match self.state(*i) {
                        (right, true) => row.with(vec![right]).set(),
                        (right, false) => row.with(vec![right]),
                    }
                }
            });
        }
        rows
    }

    /// The block as a terminal draws it: its header with the fill bar,
    /// then each line, the right column aligned as the plan's.
    pub fn lines(&self, style: Style) -> Vec<String> {
        layout(&self.rows(style, true), style)
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Where change `i` is in the tree, and the headers it is under.
    fn place(&self, i: usize) -> (usize, Vec<usize>) {
        let k = self
            .tree
            .iter()
            .position(|(_, x)| matches!(x, Item::Change(j) if *j == i))
            .expect("every change is in the tree");
        let mut headers = Vec::new();
        let mut depth = self.tree[k].0;
        for h in (0..k).rev() {
            if depth == 0 {
                break;
            }
            if self.tree[h].0 < depth {
                depth = self.tree[h].0;
                headers.push(h);
            }
        }
        headers.reverse();
        (k, headers)
    }

    /// Change `i`'s line as of now, laid out among the block's lines,
    /// after each header it is under that `said` does not hold yet (a
    /// line per change of state: a header once, before its first).
    pub fn line(&self, i: usize, style: Style, said: &mut Vec<usize>) -> Vec<String> {
        let lines: Vec<String> = layout(&self.rows(style, false), style)
            .lines()
            .map(str::to_string)
            .collect();
        let (k, headers) = self.place(i);
        let mut out = Vec::new();
        for h in headers {
            if !said.contains(&h) {
                said.push(h);
                out.push(lines[h + 1].trim_end().to_string());
            }
        }
        out.push(lines[k + 1].trim_end().to_string());
        out
    }

    /// Change `i`'s line as its call starts: its mark and name alone.
    pub fn started(&self, i: usize, style: Style, said: &mut Vec<usize>) -> Vec<String> {
        let mut out = self.line(i, style, said);
        if let Some(last) = out.last_mut() {
            let e = &self.entries[i];
            let (_, painted) = Block::mark(e, style);
            let indent = "  ".repeat(self.tree[self.place(i).0].0 + 1);
            *last = format!("{indent}{painted} {}", e.printed);
        }
        out
    }

    /// The tick's end where no bar says it: `tick 1  done 4.2s`.
    pub fn ended(&self) -> String {
        format!(
            "tick {}  {} {}",
            self.tick,
            self.outcome().unwrap_or("done"),
            took(self.elapsed())
        )
    }

    /// Change `i`'s state as `--json` would say it: `{tick, address,
    /// state, elapsed, status}`.
    pub fn json(&self, i: usize) -> Json {
        let e = &self.entries[i];
        let state = match &e.state {
            State::Waiting(_) => "waiting",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Interrupted => "interrupted",
        };
        let elapsed = e
            .took
            .or_else(|| e.started.map(|s| s.elapsed()))
            .map(|d| d.as_secs_f64());
        serde_json::json!({
            "tick": self.tick,
            "address": e.addr.to_string(),
            "state": state,
            "elapsed": elapsed,
            "status": e.status,
        })
    }
}

/// What action `a` reads that another resource computes, as the plan
/// prints it (`k3s.server.public_ip`): the first such value in its
/// changes, which a change of the tick makes before it.
fn reads(a: &Action) -> Option<String> {
    fn find(v: &Json) -> Option<String> {
        match (marker(v), v) {
            (Some((NULL_KEY, l)), _) => Some(attribute_label(l)),
            (Some(_), _) => None,
            (None, Json::Array(xs)) => xs.iter().find_map(find),
            (None, Json::Object(m)) => m.values().find_map(find),
            _ => None,
        }
    }
    a.changes
        .iter()
        .find_map(|c| c.after.as_ref().and_then(find))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(kind: ActionKind, t: &str, n: &str) -> Action {
        Action {
            kind,
            addr: Address {
                typ: t.into(),
                name: n.into(),
            },
            changes: Vec::new(),
            on: Default::default(),
            kept: Vec::new(),
        }
    }

    #[test]
    fn the_mark_alone_is_painted() {
        let a = action(ActionKind::Create, "net.vpc", "a");
        let d = action(ActionKind::Delete, "net.vpc", "d");
        let mut block = Block::new(1, &[&a, &d]);
        let color = Style { color: true };
        let mut said = Vec::new();
        assert_eq!(
            block.started(0, color, &mut said),
            ["  \x1b[32m+\x1b[0m net.vpc a"]
        );
        assert_eq!(
            block.started(1, color, &mut said),
            ["  \x1b[31m-\x1b[0m net.vpc d"]
        );
        block.start(&a.addr);
        block.fail(&a.addr, Failure::of("apply", &a.addr, "refused", "no"));
        let line = &block.line(0, color, &mut said)[0];
        assert!(
            line.starts_with("  \x1b[31m!\x1b[0m net.vpc a  "),
            "{line:?}"
        );
        // The status once the call is over is set, not dim: no colour.
        assert!(line.ends_with("failed 0.0s"), "{line:?}");
        // The terminal's header: bold as the plan's, its bar unpainted.
        block.start(&d.addr);
        block.done(&d.addr);
        block.end();
        let head = &block.lines(color)[0];
        assert!(
            head.starts_with("\x1b[1mtick 1  2 changes\x1b[0m  ━━━━━━━━━━━━  failed "),
            "{head:?}"
        );
    }

    #[test]
    fn a_duration_says_what_matters() {
        assert_eq!(took(Duration::from_millis(800)), "0.8s");
        assert_eq!(took(Duration::from_secs(42)), "42s");
        assert_eq!(took(Duration::from_secs(72)), "1m12s");
        assert_eq!(took(Duration::from_secs(3780)), "1h3m");
    }

    #[test]
    fn the_bar_fills_with_the_calls_answered() {
        assert_eq!(bar(0, 3), "░░░░░░░░░░░░");
        assert_eq!(bar(2, 3), "━━━━━━━━░░░░");
        assert_eq!(bar(3, 3), "━━━━━━━━━━━━");
        assert_eq!(bar(0, 0), "━━━━━━━━━━━━");
    }

    /// The sketch of 2026-10-08: a running call's word and time, a
    /// finished one's, a waiting one's wait; the header's bar and count;
    /// a failure said once below the block.
    #[test]
    fn each_line_says_its_call_and_the_header_fills() {
        let a = action(ActionKind::Update, "k8s.deployment", "forgejo.server");
        let b = action(ActionKind::Create, "k8s.secret", "synapse.homeserver");
        let c = action(ActionKind::Create, "k8s.service", "synapse_db.svc");
        let mut block = Block::new(1, &[&a, &b, &c]);
        block.start(&a.addr);
        block.done(&a.addr);
        block.start(&b.addr);
        let plain = |block: &Block| -> Vec<String> {
            block
                .lines(Style::default())
                .into_iter()
                .map(|l| {
                    l.split(' ')
                        .map(|w| {
                            match w.ends_with('s') && w[..w.len() - 1].parse::<f64>().is_ok() {
                                true => "T",
                                false => w,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        };
        assert_eq!(
            plain(&block),
            [
                "tick 1  3 changes                  ━━━━░░░░░░░░  1 of 3  T",
                "  ~ k8s.deployment forgejo.server  updated T",
                "  + k8s.secret synapse.homeserver  making T",
                "  + k8s.service synapse_db.svc",
            ]
        );
        // The provider's own word, verbatim, while its call runs.
        block.entries[1].status = Some("BUILD".into());
        assert!(plain(&block)[2].ends_with("BUILD T"), "{:?}", plain(&block));
        block.done(&b.addr);
        block.sites.insert(c.addr.clone(), "synapse.df:40".into());
        block.start(&c.addr);
        // The provider names the change in its own form: dform says it as
        // the plan does, once, below the block.
        block.fail(
            &c.addr,
            Failure::of(
                "apply",
                &c.addr,
                "refused, nothing changed",
                "apply k8s.service[\"synapse_db.svc\"]: 403 Forbidden",
            ),
        );
        block.end();
        let lines = plain(&block);
        assert_eq!(
            lines[0],
            "tick 1  3 changes                  ━━━━━━━━━━━━  failed T"
        );
        assert_eq!(lines[3], "  ! k8s.service synapse_db.svc     failed T");
        assert!(!lines.iter().any(|l| l.contains("403")), "{lines:?}");
        assert_eq!(
            block.failures(Style::default()),
            [
                "! apply k8s.service synapse_db.svc: refused, nothing changed",
                "    403 Forbidden",
                "    synapse.df:40",
            ]
        );
        assert!(block.ended().starts_with("tick 1  failed "));
    }

    /// A copy's changes nest under its header, as the plan's do; a line
    /// per change of state says a header once, before its first.
    #[test]
    fn a_copy_nests_as_the_plan_nests_it() {
        let a = action(ActionKind::Create, "net.vpc", "edge");
        let b = action(ActionKind::Create, "net.subnet", "k3s.agent-0.sub");
        let c = action(ActionKind::Create, "ovh.instance", "k3s.agent-0.vm");
        let mut block = Block::new(1, &[&a, &b, &c]);
        let copy = Address {
            typ: "k3s.node".into(),
            name: "k3s.agent-0".into(),
        };
        let d = |x: &Action| crate::report::Deformation {
            kind: x.kind.clone(),
            addr: x.addr.clone(),
            lines: vec![],
            site: None,
            because: None,
            custody: None,
            forces: vec![],
            folded: vec![],
            gone: None,
            kept: vec![],
        };
        let (da, db, dc) = (d(&a), d(&b), d(&c));
        let outline = [
            (0, Node::Change(&da)),
            (
                0,
                Node::Header {
                    addr: copy,
                    kind: ActionKind::Create,
                },
            ),
            (1, Node::Change(&db)),
            (1, Node::Change(&dc)),
        ];
        block = block.nested(&outline);
        let mut said = Vec::new();
        block.start(&c.addr);
        assert_eq!(
            block.started(2, Style::default(), &mut said),
            [
                "  + k3s.node k3s.agent-0",
                "    + ovh.instance k3s.agent-0.vm"
            ]
        );
        block.start(&b.addr);
        assert_eq!(
            block.started(1, Style::default(), &mut said),
            ["    + net.subnet k3s.agent-0.sub"]
        );
    }
}
