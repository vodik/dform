//! Apply's progress on stderr (R-127, R-206), as the block
//! `report::progress` renders. A [`Printer`] over the tick's [`Event`]s,
//! each with the time it happened, writes it: on a terminal its lines
//! change in place, about once a second, by moving the cursor over the
//! block's own lines and clearing each as it is written again (`crossterm`
//! for the cursor and the clear; nothing else of the screen is touched),
//! its header's bar filling; under `--yes` or with no terminal, no bar:
//! a change's line once its call answered, a running change's line again
//! every [`BEAT`] as a heartbeat, and the tick's end; under `-q` only the
//! tick's end. A line said while the block is drawn (a retry, a stop
//! asked for, a `DFORM_LOG=debug` line) is the printer's too: above the block, which a terminal
//! draws again below it. Between ticks, on a terminal, the tick's wait is
//! one line counting up. [`Progress`] is the driver: the printer on
//! stderr, the clock that beats it, the signal it says.
//!
//! Ctrl-C (or SIGTERM) while a tick runs is a request to stop
//! (`interrupt`, R-137): the block says so above it, no new change
//! starts, the calls in flight are awaited, and the block ends with what
//! never started `interrupted`; the apply unwinds from there and says the
//! next apply (a destroy's: the next destroy) resumes it. Ctrl-C again
//! quits at once.

pub use dform_core::progress::*;

use crossterm::{QueueableCommand, cursor, terminal};
use dform_core::interrupt;
use dform_core::ir::Address;
use dform_core::report::progress::{Block, State, took};
use dform_core::report::{Failure, Style};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often a running change says so again when stderr is not a
/// terminal; `DFORM_HEARTBEAT_MS` sets it (for tests).
pub const BEAT: Duration = Duration::from_secs(30);

/// How often a terminal's block is drawn again while nothing changes.
const REDRAW: Duration = Duration::from_secs(1);

/// How the block is printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Redrawn in place.
    Terminal,
    /// A change's line once its call answered.
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

    /// The mode of a tick's block, at `quiet`: drawn in place where the
    /// apply asks on a terminal (stdout and stderr both one), else under
    /// `yes` (nobody watches it fill: a log) a line per change.
    pub fn of_block(quiet: bool, yes: bool) -> Mode {
        use std::io::IsTerminal;
        match Mode::of_stderr(quiet) {
            Mode::Terminal if yes || !std::io::stdout().is_terminal() => Mode::Lines,
            m => m,
        }
    }
}

/// What the block is told, in the order it happens: the printer's whole
/// input. The tick's start is the printer's own ([`Printer::tick`]).
#[derive(Debug, Clone)]
pub enum Event {
    /// A change's call was submitted.
    Started(Address),
    /// The provider said a status word for a change's call (R-130).
    Status(Address, String),
    /// A change's call answered.
    Done(Address),
    /// A change's call failed, as it may be printed.
    Failed(Address, Failure),
    /// A line said while the block is drawn: a retry, a stop asked for,
    /// a `DFORM_LOG=debug` line.
    Note(String),
    /// The clock: a terminal's block drawn again, a running change's
    /// heartbeat elsewhere, when one is due.
    Clock,
    /// The tick ended; `interrupted`, what never started never will.
    Ended { interrupted: bool },
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
    fn lines(&self, style: Style, now: Instant) -> Vec<String> {
        match self {
            Board::Tick(b) => b.lines(style),
            Board::Wait { tick, on, since } => {
                // An extern's call has not answered: it said "not yet".
                let why = match on.iter().any(|n| n.contains('(')) {
                    true => "  not yet",
                    false => "",
                };
                let waited = now.saturating_duration_since(*since).as_secs();
                vec![
                    format!("tick {tick}"),
                    format!(
                        "  waits on  {}    {}{why}",
                        on.join(", "),
                        took(Duration::from_secs(waited))
                    ),
                ]
            }
        }
    }
}

/// The block's printer: the [`Event`]s of a tick (or the wait before
/// one), each at the time it happened, written as `mode` says.
pub struct Printer {
    board: Board,
    mode: Mode,
    style: Style,
    /// The terminal's width: a line wider is cut, so it never wraps.
    width: usize,
    /// Lines drawn on the terminal, to move back over.
    drawn: usize,
    /// When the terminal was last drawn.
    drew: Instant,
    /// When each change's line was last printed, or its call started
    /// (`Lines`): its heartbeat's clock.
    said: Vec<Instant>,
    /// The headers printed (`Lines`): each once, before its first change.
    headers: Vec<usize>,
    beat: Duration,
    /// Lines to print once, above the block (on a terminal, at its next
    /// draw).
    notes: Vec<String>,
    now: Instant,
}

impl Printer {
    /// The printer of tick `block`, started `at`; on a terminal drawn over
    /// the `asked` lines above it (the tick's question, asked on its
    /// header line: the header with its bar takes its place).
    pub fn tick(
        block: Block,
        mode: Mode,
        style: Style,
        asked: usize,
        at: Instant,
        out: &mut dyn Write,
    ) -> Printer {
        let n = block.entries.len();
        let header = block.title();
        let mut p = Printer::new(Board::Tick(block), mode, style, n, at);
        if mode == Mode::Terminal {
            p.drawn = asked;
        }
        // Apart from what is above it, unless it takes the question's
        // place.
        if mode != Mode::Quiet && p.drawn == 0 {
            let _ = writeln!(out);
        }
        match mode {
            Mode::Lines => {
                let _ = writeln!(out, "{header}");
            }
            Mode::Terminal => p.draw(out),
            Mode::Quiet => {}
        }
        p
    }

    /// The printer of the wait before tick `tick` on `on`, started `at`: a
    /// terminal's line counting up; elsewhere the wait's own lines say it,
    /// and this prints only the notes.
    pub fn wait(
        tick: usize,
        on: Vec<String>,
        mode: Mode,
        style: Style,
        at: Instant,
        out: &mut dyn Write,
    ) -> Printer {
        let board = Board::Wait {
            tick,
            on,
            since: at,
        };
        let mode = match mode {
            Mode::Terminal => Mode::Terminal,
            _ => Mode::Quiet,
        };
        let mut p = Printer::new(board, mode, style, 0, at);
        if mode == Mode::Terminal {
            p.draw(out);
        }
        p
    }

    fn new(board: Board, mode: Mode, style: Style, n: usize, at: Instant) -> Printer {
        Printer {
            board,
            mode,
            style,
            width: 100,
            drawn: 0,
            drew: at,
            said: vec![at; n],
            headers: Vec::new(),
            beat: BEAT,
            notes: Vec::new(),
            now: at,
        }
    }

    /// A running change says so again every `beat` (`Lines`).
    pub fn beat(mut self, beat: Duration) -> Printer {
        self.beat = beat;
        self
    }

    /// The terminal is `width` wide (a terminal that says none, a pty
    /// nobody sized, is a page's: 100).
    pub fn width(mut self, width: Option<usize>) -> Printer {
        self.set_width(width);
        self
    }

    fn set_width(&mut self, width: Option<usize>) {
        self.width = width.filter(|w| *w > 0).unwrap_or(100);
    }

    /// The changes that failed, in the order they failed.
    pub fn failed(&self) -> Vec<Address> {
        match &self.board {
            Board::Tick(b) => b.errors.iter().map(|(a, _)| a.clone()).collect(),
            Board::Wait { .. } => Vec::new(),
        }
    }

    /// Event `e`, which happened `at`, written to `out`.
    pub fn event(&mut self, e: Event, at: Instant, out: &mut dyn Write) {
        self.now = self.now.max(at);
        if let Board::Tick(b) = &mut self.board {
            b.clock(at);
        }
        match e {
            Event::Note(line) => self.note(line, out),
            Event::Clock => self.clock(at, out),
            Event::Ended { interrupted } => self.end(interrupted, at, out),
            e => self.change(e, at, out),
        }
    }

    /// A change's call started, said its word, answered or failed.
    fn change(&mut self, e: Event, at: Instant, out: &mut dyn Write) {
        let Board::Tick(b) = &mut self.board else {
            return;
        };
        let (addr, answered) = match &e {
            Event::Started(a) => {
                b.start(a, at);
                (a, false)
            }
            Event::Status(a, status) => {
                if let Some(x) = b.entries.iter_mut().find(|x| x.addr == *a) {
                    x.status = Some(status.clone());
                }
                (a, false)
            }
            Event::Done(a) => {
                b.done(a, at);
                (a, true)
            }
            Event::Failed(a, f) => {
                b.fail(a, f.clone(), at);
                (a, true)
            }
            _ => return,
        };
        let Some(i) = b.entries.iter().position(|x| x.addr == *addr) else {
            return;
        };
        match self.mode {
            Mode::Terminal => self.draw(out),
            // A change is said once its call answered; while it runs, the
            // heartbeat says it.
            Mode::Lines if answered => self.say(i, out),
            Mode::Lines => {
                if matches!(e, Event::Started(_)) {
                    self.said[i] = at;
                }
            }
            Mode::Quiet => {}
        }
    }

    /// A line above the block: on a terminal at its next draw, which
    /// follows at once; elsewhere as it comes.
    fn note(&mut self, line: String, out: &mut dyn Write) {
        match self.mode {
            Mode::Terminal => {
                self.notes.push(line);
                self.draw(out);
            }
            _ => {
                let _ = writeln!(out, "{line}");
            }
        }
    }

    /// The clock: a terminal's block drawn again once a [`REDRAW`] has
    /// passed; elsewhere each running change whose heartbeat is due said
    /// again.
    fn clock(&mut self, at: Instant, out: &mut dyn Write) {
        match self.mode {
            Mode::Terminal if at.saturating_duration_since(self.drew) >= REDRAW => self.draw(out),
            Mode::Lines => {
                let Board::Tick(b) = &self.board else {
                    return;
                };
                let due: Vec<usize> = (0..b.entries.len())
                    .filter(|&i| {
                        b.entries[i].state == State::Running
                            && at.saturating_duration_since(self.said[i]) >= self.beat
                    })
                    .collect();
                for i in due {
                    self.say(i, out);
                }
            }
            _ => {}
        }
    }

    /// The tick ended: the block as it ended (what never started
    /// `interrupted`, when `interrupted`), its end line, and each failure
    /// in full below it, once (R-109's shape, the address as the plan
    /// prints it).
    fn end(&mut self, interrupted: bool, at: Instant, out: &mut dyn Write) {
        let style = self.style;
        let Board::Tick(b) = &mut self.board else {
            return;
        };
        b.end(at);
        if interrupted {
            let waiting: Vec<usize> = (0..b.entries.len())
                .filter(|i| matches!(b.entries[*i].state, State::Waiting(_)))
                .collect();
            b.interrupt();
            if self.mode == Mode::Lines {
                for i in waiting {
                    self.say(i, out);
                }
            }
        }
        // On a terminal the header says how the tick ended; elsewhere its
        // own line does, but of a tick with no call: its header said it
        // all (`tick 2  nothing to do`).
        match (self.mode, &self.board) {
            (Mode::Terminal, _) => self.draw(out),
            (Mode::Lines, Board::Tick(b)) if b.entries.is_empty() => {}
            (_, Board::Tick(b)) => {
                let _ = writeln!(out, "{}", b.ended());
            }
            _ => {}
        }
        if let Board::Tick(b) = &self.board {
            for line in b.failures(style) {
                let _ = writeln!(out, "{line}");
            }
        }
    }

    /// Draw the board over the lines drawn before, the notes above it.
    fn draw(&mut self, out: &mut dyn Write) {
        if self.drawn > 0 {
            let _ = out.queue(cursor::MoveToPreviousLine(self.drawn as u16));
        }
        // Printed where the block began: the block is drawn below them.
        for note in std::mem::take(&mut self.notes) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = writeln!(out, "{note}");
        }
        let lines = self.board.lines(self.style, self.now);
        let plain = self.board.lines(Style::default(), self.now);
        for (l, p) in lines.iter().zip(&plain) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            // A line wider than the terminal would wrap and throw the
            // count off: it says less, unpainted.
            let text: String = match p.chars().count() >= self.width {
                true => p.chars().take(self.width.saturating_sub(1)).collect(),
                false => l.clone(),
            };
            let _ = writeln!(out, "{text}");
        }
        let _ = out.flush();
        self.drawn = lines.len();
        self.drew = self.now;
    }

    /// Print change `i`'s line as of now (`Lines`), after each header it
    /// is under not said yet.
    fn say(&mut self, i: usize, out: &mut dyn Write) {
        if let Board::Tick(b) = &self.board {
            for l in b.line(i, self.style, &mut self.headers) {
                let _ = writeln!(out, "{l}");
            }
            self.said[i] = self.now;
        }
    }
}

/// The terminal's width, if it says one.
fn terminal_width() -> Option<usize> {
    terminal::size().ok().map(|(w, _)| w as usize)
}

/// The heartbeat: [`BEAT`], else `DFORM_HEARTBEAT_MS`.
fn beat() -> Duration {
    std::env::var("DFORM_HEARTBEAT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(BEAT)
}

/// The live block of one tick, or of the wait after it: its printer on
/// stderr, beaten by a clock of its own, which also says a stop asked
/// for; a line said meanwhile (`progress::line`) is the printer's.
pub struct Progress {
    printer: Arc<Mutex<Printer>>,
    /// The stop was said.
    stopping: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Progress {
    /// Start printing tick `block`; on a terminal over the `asked` lines
    /// above it.
    pub fn tick(block: Block, mode: Mode, style: Style, asked: usize) -> Progress {
        let mut err = std::io::stderr().lock();
        let p = Printer::tick(block, mode, style, asked, Instant::now(), &mut err)
            .beat(beat())
            .width(terminal_width());
        Progress::start(p)
    }

    /// Start printing the wait before tick `tick` on `on` (a terminal
    /// only: elsewhere the wait's own lines say it).
    pub fn wait(tick: usize, on: Vec<String>, mode: Mode, style: Style) -> Progress {
        let mut err = std::io::stderr().lock();
        let p =
            Printer::wait(tick, on, mode, style, Instant::now(), &mut err).width(terminal_width());
        Progress::start(p)
    }

    fn start(printer: Printer) -> Progress {
        let printer = Arc::new(Mutex::new(printer));
        let stop: Arc<AtomicBool> = Arc::default();
        let stopping: Arc<AtomicBool> = Arc::default();
        {
            let printer = printer.clone();
            route(Some(Box::new(move |l: &str| {
                send(&printer, Event::Note(l.to_string()))
            })));
        }
        let thread = {
            let (printer, stop, stopping) = (printer.clone(), stop.clone(), stopping.clone());
            std::thread::spawn(move || watch(&printer, &stop, &stopping))
        };
        Progress {
            printer,
            stopping,
            stop,
            thread: Some(thread),
        }
    }

    /// The executor's event `e`; `redact` says an error or a word as it
    /// may be printed.
    pub fn event(&self, e: &dform_core::executor::Event, redact: &dyn Fn(&str) -> String) {
        use dform_core::executor::Event as E;
        let e = match e {
            E::Started(a) => Event::Started((*a).clone()),
            E::Finished(a) => Event::Done((*a).clone()),
            E::Failed(a, err) => Event::Failed((*a).clone(), failure(a, err, redact)),
            // The provider's status word, beside the change as it says it
            // (R-130); an event with none changes nothing shown.
            E::Progress(a, e) => match &e.status {
                Some(status) => Event::Status((*a).clone(), redact(status)),
                None => return,
            },
        };
        send(&self.printer, e);
    }

    /// The tick ended: a stop asked for since the clock last looked is
    /// said first; then the block as it ended. The failed changes, in the
    /// order they failed.
    pub fn finish(mut self) -> Vec<Address> {
        self.halt();
        say_stop(&self.printer, &self.stopping);
        let interrupted = interrupt::requested().is_some();
        send(&self.printer, Event::Ended { interrupted });
        self.printer.lock().expect("progress").failed()
    }

    /// The wait ended.
    pub fn done(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
            route(None);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.halt();
    }
}

/// `e` to the printer, now, on stderr.
fn send(printer: &Mutex<Printer>, e: Event) {
    let mut p = printer.lock().expect("progress");
    if p.mode == Mode::Terminal {
        p.set_width(terminal_width());
    }
    p.event(e, Instant::now(), &mut std::io::stderr().lock());
}

/// A change's failure in R-109's shape, as it may be printed: the
/// provider's or the core's, else the error as it is under `apply T A`.
fn failure(addr: &Address, err: &anyhow::Error, redact: &dyn Fn(&str) -> String) -> Failure {
    let f = match err.downcast_ref::<Failure>() {
        Some(f) => f.clone(),
        None => Failure::of("apply", addr, "failed", &format!("{err:#}")),
    };
    Failure {
        what: redact(&f.what),
        message: redact(&f.message),
        ..f
    }
}

/// The driver's own clock: the printer's every 50ms; a stop asked for,
/// said.
fn watch(printer: &Mutex<Printer>, stop: &AtomicBool, stopping: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        say_stop(printer, stopping);
        send(printer, Event::Clock);
    }
}

/// A stop was asked for: said once, above the block.
fn say_stop(printer: &Mutex<Printer>, stopping: &AtomicBool) {
    let Some(sig) = interrupt::requested() else {
        return;
    };
    if stopping.swap(true, Ordering::SeqCst) {
        return;
    }
    send(printer, Event::Note(stop_text(sig)));
}

/// `Ctrl-C: stopping after the calls in flight; Ctrl-C again to quit
/// now`, for signal `sig`.
pub fn stop_text(sig: i32) -> String {
    let what = match sig {
        libc::SIGINT => "Ctrl-C",
        _ => interrupt::name(sig),
    };
    format!("{what}: stopping after the calls in flight; Ctrl-C again to quit now")
}
