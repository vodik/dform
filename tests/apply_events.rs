//! The apply's block is a printer over the tick's events (R-127, R-206,
//! the house rule: output is a printer over events): a fixed list of
//! them, each at a fixed time, prints the same every time, with and
//! without a terminal. With none (`--yes`, a pipe, CI): no bar, a
//! change's line once its call answered with its word and time, a running
//! one's line again every heartbeat (the provider's own word in it, R-130),
//! the tick's end, each failure once below; `-q` the end alone; on a
//! terminal the block drawn in place, the header's bar filling, a line
//! said meanwhile (a retry, the stop) above it with no stale copy of the
//! block left. That the events reach the printer from every backend is
//! the last test's (a provider launched).

mod common;
use common::{BACKENDS, Run, Scratch};
use dform::ir::Address;
use dform::progress::{Event, Mode, Printer, stop_text};
use dform::provider::{Action, ActionKind};
use dform::report::progress::Block;
use dform::report::{Failure, Style};
use std::time::{Duration, Instant};

fn addr(t: &str, n: &str) -> Address {
    Address {
        typ: t.into(),
        name: n.into(),
    }
}

fn action(t: &str, n: &str) -> Action {
    Action {
        kind: ActionKind::Create,
        addr: addr(t, n),
        changes: Vec::new(),
        on: Default::default(),
        kept: Vec::new(),
    }
}

/// The tick of `vpc main`, `subnet a`, `subnet b`, started at `t0`, the
/// subnets' failures said at `p.df:4`, `p.df:5`.
fn block(t0: Instant) -> Block {
    let (v, a, b) = (
        action("net.vpc", "main"),
        action("net.subnet", "a"),
        action("net.subnet", "b"),
    );
    let mut block = Block::new(1, &[&v, &a, &b], t0);
    block.sites.insert(a.addr.clone(), "p.df:4".into());
    block.sites.insert(b.addr.clone(), "p.df:5".into());
    block
}

/// What the printer of `block` in `mode`, its heartbeat `beat`, wrote
/// for `events`, each `ms` after `t0` (the tick's question, `asked`
/// lines, above it).
fn print(block: Block, mode: Mode, asked: usize, t0: Instant, events: Vec<(u64, Event)>) -> String {
    let mut out = Vec::new();
    let mut p = Printer::tick(block, mode, Style::default(), asked, t0, &mut out)
        .beat(Duration::from_secs(30))
        .width(Some(80));
    for (ms, e) in events {
        p.event(e, t0 + Duration::from_millis(ms), &mut out);
    }
    String::from_utf8(out).unwrap()
}

/// The vpc made, `a` slow (the provider says `made` while it answers
/// late), `b` failing.
fn tick(at_end: Event) -> Vec<(u64, Event)> {
    let (v, a, b) = (
        addr("net.vpc", "main"),
        addr("net.subnet", "a"),
        addr("net.subnet", "b"),
    );
    vec![
        (0, Event::Started(v.clone())),
        (120, Event::Done(v)),
        (120, Event::Started(a.clone())),
        (150, Event::Status(a.clone(), "made".into())),
        (10_000, Event::Clock),
        (30_200, Event::Clock),
        (30_250, Event::Clock),
        (45_000, Event::Done(a)),
        (45_000, Event::Started(b.clone())),
        (
            45_300,
            Event::Failed(
                b.clone(),
                Failure::of(
                    "apply",
                    &b,
                    "refused, nothing changed",
                    "injected failure (chaos fail=net.subnet b)",
                ),
            ),
        ),
        (45_400, at_end),
    ]
}

/// No terminal (`--yes`, a pipe): no bar, no line as a call starts; each
/// change once its call answered, a running one again at each heartbeat
/// with the provider's word; the end; the failure once below.
#[test]
fn without_a_terminal_a_change_is_said_once_it_answered() {
    let t0 = Instant::now();
    let out = print(
        block(t0),
        Mode::Lines,
        0,
        t0,
        tick(Event::Ended { interrupted: false }),
    );
    assert_eq!(
        out,
        "\n\
         tick 1  3 changes\n  \
         + net.vpc main  made 0.1s\n  \
         + net.subnet a  made 30s\n  \
         + net.subnet a  made 44s\n  \
         ! net.subnet b  failed 0.3s\n\
         tick 1  failed 45s\n\
         ! apply net.subnet b: refused, nothing changed\n    \
         injected failure (chaos fail=net.subnet b)\n    \
         p.df:5\n"
    );
}

/// `-q`: the tick's end alone, and the failure.
#[test]
fn quiet_says_the_end_alone() {
    let t0 = Instant::now();
    let out = print(
        block(t0),
        Mode::Quiet,
        0,
        t0,
        tick(Event::Ended { interrupted: false }),
    );
    assert_eq!(
        out,
        "tick 1  failed 45s\n\
         ! apply net.subnet b: refused, nothing changed\n    \
         injected failure (chaos fail=net.subnet b)\n    \
         p.df:5\n"
    );
}

/// A stop asked for while `a` runs: said as it comes, `a` awaited, what
/// never started `interrupted`, the tick's end.
#[test]
fn an_interrupt_ends_the_block_with_what_never_started() {
    let t0 = Instant::now();
    let (v, a) = (addr("net.vpc", "main"), addr("net.subnet", "a"));
    let out = print(
        block(t0),
        Mode::Lines,
        0,
        t0,
        vec![
            (0, Event::Started(v.clone())),
            (100, Event::Done(v)),
            (100, Event::Started(a.clone())),
            (150, Event::Note(stop_text(libc::SIGINT))),
            (300, Event::Done(a)),
            (300, Event::Ended { interrupted: true }),
        ],
    );
    assert_eq!(
        out,
        "\n\
         tick 1  3 changes\n  \
         + net.vpc main  made 0.1s\n\
         Ctrl-C: stopping after the calls in flight; Ctrl-C again to quit now\n  \
         + net.subnet a  made 0.2s\n  \
         + net.subnet b  interrupted\n\
         tick 1  interrupted 0.3s\n"
    );
}

/// What a terminal shows of `out`: each line as the cursor left it, the
/// cursor moved up (`ESC [ n F`) and lines cleared (`ESC [ 2 K`) as the
/// printer asks.
fn screen(out: &str) -> Vec<String> {
    let mut lines: Vec<String> = vec![String::new()];
    let mut row = 0;
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                chars.next(); // '['
                let mut n = String::new();
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    n.push(d);
                }
                match chars.next() {
                    Some('F') => row -= n.parse::<usize>().unwrap_or(1),
                    Some('K') => lines[row].clear(),
                    x => panic!("an escape the printer does not write: {x:?}"),
                }
            }
            '\n' => {
                row += 1;
                if row == lines.len() {
                    lines.push(String::new());
                }
            }
            c => lines[row].push(c),
        }
    }
    lines.pop_if(|l| l.is_empty());
    lines
}

/// On a terminal: the block drawn in place, the header's bar filling
/// (calls answered over the tick's), each line its call's word and time;
/// at its end `done` and the tick's time.
#[test]
fn on_a_terminal_the_block_fills_in_place() {
    let t0 = Instant::now();
    let (v, a, b) = (
        addr("net.vpc", "main"),
        addr("net.subnet", "a"),
        addr("net.subnet", "b"),
    );
    let mut events = vec![
        (0, Event::Started(v.clone())),
        (100, Event::Done(v)),
        (100, Event::Started(a.clone())),
        (2_100, Event::Clock),
    ];
    let running = print(block(t0), Mode::Terminal, 0, t0, events.clone());
    assert_eq!(
        screen(&running),
        [
            "",
            "tick 1  3 changes  ━━━━░░░░░░░░  1 of 3  2.1s",
            "  + net.vpc main   made 0.1s",
            "  + net.subnet a   making 2.0s",
            "  + net.subnet b",
        ]
    );
    events.extend([
        (2_500, Event::Done(a)),
        (2_500, Event::Started(b.clone())),
        (2_700, Event::Done(b)),
        (2_700, Event::Ended { interrupted: false }),
    ]);
    let done = print(block(t0), Mode::Terminal, 0, t0, events);
    assert_eq!(
        screen(&done),
        [
            "",
            "tick 1  3 changes  ━━━━━━━━━━━━  done 2.7s",
            "  + net.vpc main   made 0.1s",
            "  + net.subnet a   made 2.4s",
            "  + net.subnet b   made 0.2s",
        ]
    );
}

/// A line said while the block is drawn (a retry; the mock's late
/// answer once was one) goes above the block, which is drawn again below
/// it: no stale header is left above.
#[test]
fn a_line_said_meanwhile_goes_above_the_block() {
    let t0 = Instant::now();
    let v = addr("net.vpc", "main");
    let out = print(
        block(t0),
        Mode::Terminal,
        0,
        t0,
        vec![
            (0, Event::Started(v.clone())),
            (
                50,
                Event::Note("retry net.vpc main apply (2/5): 503".into()),
            ),
            (100, Event::Done(v)),
        ],
    );
    assert_eq!(
        screen(&out),
        [
            "",
            "retry net.vpc main apply (2/5): 503",
            "tick 1  3 changes  ━━━━░░░░░░░░  1 of 3  0.1s",
            "  + net.vpc main   made 0.1s",
            "  + net.subnet a",
            "  + net.subnet b",
        ]
    );
}

/// A later tick asked on its header line (`tick 2  1 change   apply?
/// [y/N]`): the block's header takes the question's place.
#[test]
fn the_block_takes_the_questions_place() {
    let t0 = Instant::now();
    let vm = action("compute.vm", "app");
    let b = Block::new(2, &[&vm], t0);
    let mut out = "tick 2  1 change   apply? [y/N] y\n".as_bytes().to_vec();
    let mut p = Printer::tick(b, Mode::Terminal, Style::default(), 1, t0, &mut out).width(Some(80));
    p.event(
        Event::Started(vm.addr.clone()),
        t0 + Duration::from_millis(10),
        &mut out,
    );
    p.event(
        Event::Done(vm.addr.clone()),
        t0 + Duration::from_millis(400),
        &mut out,
    );
    p.event(
        Event::Ended { interrupted: false },
        t0 + Duration::from_millis(400),
        &mut out,
    );
    assert_eq!(
        screen(&String::from_utf8(out).unwrap()),
        [
            "tick 2  1 change    ━━━━━━━━━━━━  done 0.4s",
            "  + compute.vm app  made 0.4s",
        ]
    );
}

/// What a provider says while an Apply runs reaches dform from every
/// backend (the process's gRPC stream, the direct and wire backends'
/// linked mock): `DFORM_LOG=debug` logs each event with its message, as
/// it arrives; the block shows its word (the printer's tests above).
#[test]
fn a_providers_events_reach_dform_on_every_backend() {
    for backend in BACKENDS {
        let s = Scratch::new(&format!("apply-events-{backend:?}"));
        s.write(
            "p.df",
            "use fake\n\
             resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
             resource net.subnet a { vpc_id = ref(main), cidr = \"10.0.1.0/24\" }\n",
        );
        let mut c = backend.command();
        c.args(common::on(
            "p.df",
            &[
                "--world",
                "w.json",
                "--chaos",
                "delay=net.subnet[\"a\"]:100",
            ],
            &["apply", "--yes"],
        ))
        .env("DFORM_LOG", "debug")
        .current_dir(s.path(""));
        let r = Run::from(c.output().unwrap()).success();
        assert!(
            r.stderr
                .lines()
                .any(|l| l.ends_with("apply net.subnet a: made: answers 100ms late (chaos delay)")),
            "{backend:?}\n{}",
            r.stderr
        );
    }
}
