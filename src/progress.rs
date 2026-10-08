//! Apply's progress on stderr (R-127), as the block `report::progress`
//! renders: on a terminal its lines change in place, about once a
//! second, by moving the cursor over the block's own lines and clearing
//! each as it is written again (`crossterm` for the cursor, the clear and
//! the width; nothing else of the screen is touched); otherwise one line
//! per change of state, and a running change's line again every
//! [`BEAT`] as a heartbeat; under `-q` only the tick's end. Between
//! ticks, on a terminal, the tick's wait is one line counting up.
//!
//! Ctrl-C (or SIGTERM) while a tick runs is a request to stop
//! (`interrupt`, R-137): the block says so above it, no new change
//! starts, the calls in flight are awaited, and the block ends with what
//! never started `interrupted`; the apply unwinds from there and says the
//! next apply (a destroy's: the next destroy) resumes it. Ctrl-C again
//! quits at once.

pub use dform_core::progress::*;

use crossterm::{QueueableCommand, cursor, terminal};
use dform_core::executor::Event;
use dform_core::interrupt;
use dform_core::report::Style;
use dform_core::report::progress::{Block, took};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a running change says so again when stderr is not a
/// terminal; `DFORM_HEARTBEAT_MS` sets it (for tests).
pub const BEAT: Duration = Duration::from_secs(30);

/// How the block is printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Redrawn in place.
    Terminal,
    /// A line per change of state.
    Lines,
    /// The tick's end alone (`-q`).
    Quiet,
}

impl Mode {
    /// The mode for stderr, at `quiet`.
    pub fn of_stderr(quiet: bool) -> Mode {
        use std::io::IsTerminal;
        match (quiet, std::io::stderr().is_terminal()) {
            (true, _) => Mode::Quiet,
            (_, true) => Mode::Terminal,
            _ => Mode::Lines,
        }
    }
}

/// What is drawn: a tick's block, or the wait between ticks.
enum Board {
    Tick(Block),
    Wait {
        tick: usize,
        on: Vec<String>,
        since: Instant,
    },
}

impl Board {
    fn lines(&self, style: Style) -> Vec<String> {
        match self {
            Board::Tick(b) => b.lines(style),
            Board::Wait { tick, on, since } => {
                // An extern's call has not answered: it said "not yet".
                let why = match on.iter().any(|n| n.contains('(')) {
                    true => "  not yet",
                    false => "",
                };
                vec![
                    format!("tick {tick}"),
                    format!(
                        "  waits on  {}    {}{why}",
                        on.join(", "),
                        took(Duration::from_secs(since.elapsed().as_secs()))
                    ),
                ]
            }
        }
    }
}

struct Inner {
    board: Board,
    mode: Mode,
    style: Style,
    /// Lines drawn on the terminal, to move back over.
    drawn: usize,
    /// When each change's line was last printed (`Lines`).
    said: Vec<Instant>,
    beat: Duration,
    /// A line to print once, above the block (on a terminal, at its next
    /// redraw).
    note: Option<String>,
    /// The stop was said.
    stopping: bool,
}

impl Inner {
    /// Draw the board over the lines drawn before.
    fn redraw(&mut self) {
        let mut err = std::io::stderr().lock();
        // A terminal that says no width (a pty nobody sized) is a page's.
        let width = terminal::size()
            .map(|(w, _)| w as usize)
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or(100);
        if self.drawn > 0 {
            let _ = err.queue(cursor::MoveToPreviousLine(self.drawn as u16));
        }
        // Printed where the block began: the block is drawn below it.
        if let Some(note) = self.note.take() {
            let _ = err.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = writeln!(err, "{note}");
        }
        let lines = self.board.lines(self.style);
        let plain = self.board.lines(Style::default());
        for (l, p) in lines.iter().zip(&plain) {
            let _ = err.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            // A line wider than the terminal would wrap and throw the
            // count off: it says less, unpainted.
            let text: String = match p.chars().count() >= width {
                true => p.chars().take(width.saturating_sub(1)).collect(),
                false => l.clone(),
            };
            let _ = writeln!(err, "{text}");
        }
        let _ = err.flush();
        self.drawn = lines.len();
    }

    /// Print change `i`'s line (`Lines`): as it starts, its mark and
    /// address; after, with its time.
    fn say(&mut self, i: usize, start: bool) {
        if let Board::Tick(b) = &self.board {
            match start {
                true => eprintln!("{}", b.started(i, self.style)),
                false => eprintln!("{}", b.line(i, self.style)),
            }
            self.said[i] = Instant::now();
        }
    }
}

/// The live block of one tick, or of the wait after it.
pub struct Progress {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Progress {
    /// Start printing tick `block`.
    pub fn tick(block: Block, mode: Mode, style: Style) -> Progress {
        let n = block.entries.len();
        let header = block.title();
        let p = Progress::start(Board::Tick(block), mode, style, n);
        if mode == Mode::Lines {
            eprintln!("{header}");
        }
        p
    }

    /// Start printing the wait before tick `tick` on `on` (a terminal
    /// only: elsewhere the wait's own lines say it).
    pub fn wait(tick: usize, on: Vec<String>, mode: Mode, style: Style) -> Progress {
        let board = Board::Wait {
            tick,
            on,
            since: Instant::now(),
        };
        let mode = match mode {
            Mode::Terminal => Mode::Terminal,
            _ => Mode::Quiet,
        };
        Progress::start(board, mode, style, 0)
    }

    fn start(board: Board, mode: Mode, style: Style, n: usize) -> Progress {
        let beat = std::env::var("DFORM_HEARTBEAT_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(BEAT);
        let inner = Arc::new(Mutex::new(Inner {
            board,
            mode,
            style,
            drawn: 0,
            said: vec![Instant::now(); n],
            beat,
            note: None,
            stopping: false,
        }));
        if mode == Mode::Terminal {
            inner.lock().expect("progress").redraw();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (inner, stop) = (inner.clone(), stop.clone());
            std::thread::spawn(move || watch(&inner, &stop))
        };
        Progress {
            inner,
            stop,
            thread: Some(thread),
        }
    }

    /// An action changed state; `redact` says an error as it may be
    /// printed.
    pub fn event(&self, e: &Event, redact: &dyn Fn(&str) -> String) {
        let mut inner = self.inner.lock().expect("progress");
        let Board::Tick(b) = &mut inner.board else {
            return;
        };
        let addr = match e {
            Event::Started(a) => {
                b.start(a);
                *a
            }
            Event::Finished(a) => {
                b.done(a);
                *a
            }
            Event::Failed(a, err) => {
                b.fail(a, failure(a, err, redact));
                *a
            }
            // The provider's status word, beside the change as it says it
            // (R-130); an event with none changes nothing shown.
            Event::Progress(a, e) => {
                let Some(status) = &e.status else { return };
                if let Some(x) = b.entries.iter_mut().find(|x| x.addr == **a) {
                    x.status = Some(redact(status));
                }
                *a
            }
        };
        let i = b.entries.iter().position(|x| x.addr == *addr);
        let start = matches!(e, Event::Started(_));
        match (inner.mode, i) {
            (Mode::Terminal, _) => inner.redraw(),
            (Mode::Lines, Some(i)) => inner.say(i, start),
            _ => {}
        }
    }

    /// The tick ended: the block as it ended, its end line, and each
    /// failure in full below it, once (R-109's shape, the address as the
    /// plan prints it). Stopped by a signal, what never started is
    /// `interrupted`. The failed changes, in the order they failed.
    pub fn finish(mut self) -> Vec<dform_core::ir::Address> {
        self.halt();
        let mut inner = self.inner.lock().expect("progress");
        let (mode, style) = (inner.mode, inner.style);
        if interrupt::requested().is_some()
            && let Board::Tick(b) = &mut inner.board
        {
            for i in 0..b.entries.len() {
                if matches!(
                    b.entries[i].state,
                    dform_core::report::progress::State::Waiting(_)
                ) {
                    b.entries[i].state = dform_core::report::progress::State::Interrupted;
                    if mode == Mode::Lines {
                        eprintln!("{}", b.line(i, style));
                    }
                }
            }
        }
        if inner.mode == Mode::Terminal {
            inner.redraw();
        }
        let Board::Tick(b) = &inner.board else {
            return Vec::new();
        };
        eprintln!("{}", b.end());
        for line in b.failures(style) {
            eprintln!("{line}");
        }
        b.errors.iter().map(|(a, _)| a.clone()).collect()
    }

    /// The wait ended.
    pub fn done(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.halt();
    }
}

/// A change's failure in R-109's shape, as it may be printed: the
/// provider's or the core's, else the error as it is under `apply T A`.
fn failure(
    addr: &dform_core::ir::Address,
    err: &anyhow::Error,
    redact: &dyn Fn(&str) -> String,
) -> dform_core::report::Failure {
    use dform_core::report::Failure;
    let f = match err.downcast_ref::<Failure>() {
        Some(f) => f.clone(),
        None => Failure::of("apply", addr, "failed", &format!("{err:#}")),
    };
    Failure {
        what: redact(&f.what),
        message: redact(&f.message),
        site: f.site,
        addr: f.addr,
    }
}

/// The driver's own clock: a redraw a second on a terminal, a heartbeat
/// per running change every `beat` otherwise; a stop asked for, said.
fn watch(inner: &Mutex<Inner>, stop: &AtomicBool) {
    let mut drawn = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        let mut g = inner.lock().expect("progress");
        if let Some(sig) = interrupt::requested()
            && !g.stopping
        {
            g.stopping = true;
            stopping(&mut g, sig);
        }
        match g.mode {
            Mode::Terminal if drawn.elapsed() >= Duration::from_secs(1) => {
                g.redraw();
                drawn = Instant::now();
            }
            Mode::Lines => {
                let due: Vec<usize> = match &g.board {
                    Board::Tick(b) => (0..b.entries.len())
                        .filter(|&i| {
                            b.entries[i].state == dform_core::report::progress::State::Running
                                && g.said[i].elapsed() >= g.beat
                        })
                        .collect(),
                    Board::Wait { .. } => Vec::new(),
                };
                for i in due {
                    g.say(i, false);
                }
            }
            _ => {}
        }
    }
}

/// A stop was asked for: said once, above the block on a terminal.
fn stopping(g: &mut Inner, sig: i32) {
    let what = match sig {
        libc::SIGINT => "Ctrl-C",
        _ => interrupt::name(sig),
    };
    let note = format!("{what}: stopping after the calls in flight; Ctrl-C again to quit now");
    match g.mode {
        Mode::Terminal => {
            g.note = Some(note);
            g.redraw();
        }
        _ => eprintln!("{note}"),
    }
}
