//! Apply's progress (R-127): the tick's block from the plan, filling in.
//! Each change is one line, its mark and its address as the plan prints
//! them, then its state: what it waits on, or the time it has run (the
//! time ticking is the liveness signal: no spinner, no glyph), then the
//! provider's own status word when it streams one (verbatim, else
//! nothing). A failure's mark is `!`, the first line of its error in
//! the right column. The driver (the `dform` binary's `progress`) prints
//! it: redrawn in place on a terminal, a line per state change
//! otherwise.

use super::{Paint, Style, address, attribute_label, marker_of};
use crate::ir::Address;
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
    /// Its error's first line.
    Failed(String),
    /// Never started: the apply was interrupted first (a running one is
    /// awaited, and ends `Done` or `Failed`).
    Interrupted,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub addr: Address,
    mark: &'static str,
    printed: String,
    pub state: State,
    started: Option<Instant>,
    took: Option<Duration>,
    /// The provider's status word, as it says it.
    pub status: Option<String>,
}

/// One tick's block.
#[derive(Debug, Clone)]
pub struct Block {
    pub tick: usize,
    pub entries: Vec<Entry>,
    started: Instant,
    /// Every failure's full error, in the order they came.
    pub errors: Vec<(Address, String)>,
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

impl Block {
    /// The tick's changes, in plan order, each waiting.
    pub fn new(tick: usize, actions: &[&Action]) -> Block {
        let entries = actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Noop | ActionKind::Pending))
            .map(|a| Entry {
                addr: a.addr.clone(),
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
            entries,
            started: Instant::now(),
            errors: Vec::new(),
        }
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

    /// Its Apply call failed with `error`.
    pub fn fail(&mut self, addr: &Address, error: &str) {
        let first = error.lines().next().unwrap_or_default().to_string();
        if let Some(e) = self.entry(addr) {
            e.state = State::Failed(first);
            e.took = e.started.map(|s| s.elapsed());
        }
        self.errors.push((addr.clone(), error.to_string()));
    }

    /// The apply was interrupted: what runs is interrupted.
    pub fn interrupt(&mut self) {
        for e in &mut self.entries {
            if e.state == State::Running {
                e.state = State::Interrupted;
                e.took = e.started.map(|s| s.elapsed());
            }
        }
    }

    /// Its title, `tick 1  3 changes`.
    pub fn title(&self) -> String {
        let n = self.entries.len();
        format!(
            "tick {}  {n} change{}",
            self.tick,
            if n == 1 { "" } else { "s" }
        )
    }

    /// Its header: `tick 1  3 changes   1 done  1 running  1 waiting`.
    pub fn header(&self) -> String {
        let count = |f: fn(&State) -> bool| self.entries.iter().filter(|e| f(&e.state)).count();
        let mut out = self.title();
        let parts = [
            (count(|s| *s == State::Done), "done"),
            (count(|s| *s == State::Running), "running"),
            (count(|s| matches!(s, State::Waiting(_))), "waiting"),
            (count(|s| matches!(s, State::Failed(_))), "failed"),
            (count(|s| *s == State::Interrupted), "interrupted"),
        ];
        let mut sep = "   ";
        for (k, word) in parts {
            if k > 0 {
                out.push_str(&format!("{sep}{k} {word}"));
                sep = "  ";
            }
        }
        out
    }

    /// The width of the address column.
    fn column(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.printed.chars().count() + 4)
            .max()
            .unwrap_or(0)
    }

    /// Change `i`'s line as it starts: its mark and address alone (a
    /// line per change of state says it once; its time follows).
    pub fn started(&self, i: usize) -> String {
        let e = &self.entries[i];
        format!("  {} {}", e.mark, e.printed)
    }

    /// Change `i`'s line, its time as of now.
    pub fn line(&self, i: usize, style: Style) -> String {
        let e = &self.entries[i];
        let col = self.column();
        let mark = match e.state {
            State::Failed(_) => "!",
            _ => e.mark,
        };
        let left = format!("  {mark} {}", e.printed);
        let pad = " ".repeat(col.saturating_sub(left.chars().count()) + 2);
        let time = |e: &Entry| {
            e.took
                .or_else(|| e.started.map(|s| s.elapsed()))
                .map(took)
                .unwrap_or_default()
        };
        let right = match &e.state {
            State::Waiting(Some(on)) => format!("waits on {on}"),
            State::Waiting(None) => String::new(),
            State::Running | State::Done => match &e.status {
                Some(s) => format!("{}  {s}", time(e)),
                None => time(e),
            },
            State::Failed(err) => format!("{}  {err}", time(e)),
            State::Interrupted => format!("{}  interrupted", time(e)),
        };
        let painted = match e.state {
            State::Failed(_) => style.paint(Paint::Error, &left),
            _ => left,
        };
        match right.is_empty() {
            true => painted,
            false => format!("{painted}{pad}{}", style.paint(Paint::Dim, &right)),
        }
    }

    /// The block: its header, then each change's line.
    pub fn lines(&self, style: Style) -> Vec<String> {
        let mut out = vec![self.header()];
        out.extend((0..self.entries.len()).map(|i| self.line(i, style)));
        out
    }

    /// The tick's end: `tick 1  done  1m50s`, `tick 1  failed  ..`.
    pub fn end(&self) -> String {
        let failed = self
            .entries
            .iter()
            .any(|e| matches!(e.state, State::Failed(_)));
        let interrupted = self.entries.iter().any(|e| e.state == State::Interrupted);
        let word = match (failed, interrupted) {
            (true, _) => "failed",
            (_, true) => "interrupted",
            _ => "done",
        };
        format!(
            "tick {}  {word}  {}",
            self.tick,
            took(self.started.elapsed())
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
            State::Failed(_) => "failed",
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
        }
    }

    #[test]
    fn a_duration_says_what_matters() {
        assert_eq!(took(Duration::from_millis(800)), "0.8s");
        assert_eq!(took(Duration::from_secs(42)), "42s");
        assert_eq!(took(Duration::from_secs(72)), "1m12s");
        assert_eq!(took(Duration::from_secs(3780)), "1h3m");
    }

    #[test]
    fn the_block_counts_its_changes_by_state() {
        let a = action(ActionKind::Create, "ovh.ssh_key", "k3s.admin");
        let b = action(ActionKind::Create, "ovh.instance", "k3s.server");
        let c = action(ActionKind::Create, "ovh.domain_record", "k3s.dns");
        let mut block = Block::new(1, &[&a, &b, &c]);
        block.start(&a.addr);
        block.done(&a.addr);
        block.start(&b.addr);
        assert_eq!(
            block.header(),
            "tick 1  3 changes   1 done  1 running  1 waiting"
        );
        block.fail(&b.addr, "403 Forbidden\nmore");
        let line = block.line(1, Style::default());
        assert!(line.starts_with("  ! ovh.instance k3s.server"), "{line}");
        assert!(line.ends_with("  403 Forbidden"), "{line}");
        assert!(block.end().starts_with("tick 1  failed  "));
    }
}
