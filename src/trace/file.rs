//! The trace file, in every build: each span as OTLP's JSON encoding
//! has it, the spans ended so far written as one line, an
//! `ExportTraceServiceRequest` (`resourceSpans`, `scopeSpans`, `spans`),
//! as OpenTelemetry's file exporter writes them and Jaeger loads them.
//! Written here from the span data, with no OpenTelemetry crate: a span's
//! name is its `otel.name`, its kind its `otel.kind` (`client` for a
//! call, else internal), every other field an attribute.
//!
//! [`Writer`] is the subscriber that feeds it; with the `otel` feature
//! and an endpoint, [`Layer`] feeds it from the same subscriber as the
//! exporter.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata};

/// The spans ended this many at a time make a line.
const LINE: usize = 512;

/// The spans of a run: those open, and those ended and not yet written.
pub struct Spans {
    state: Mutex<State>,
}

struct State {
    out: std::io::BufWriter<std::fs::File>,
    open: HashMap<u64, Open>,
    ended: Vec<Value>,
    trace_id: String,
    /// Span ids: a counter mixed with this run's seed.
    seed: RandomState,
    next: u64,
}

/// A span open: what its line will say.
struct Open {
    span_id: String,
    /// Its parent's span id: taken when it begins, the parent may end
    /// first (a call answered after its phase).
    parent: String,
    name: String,
    client: bool,
    start: u128,
    attributes: Vec<Value>,
}

/// `n` random bits, hex.
fn hex(seed: &RandomState, n: u64) -> String {
    format!("{:016x}", seed.hash_one(n))
}

fn now() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

impl Spans {
    /// The trace file at `path`, made empty.
    pub fn create(path: &Path) -> std::io::Result<Spans> {
        let seed = RandomState::new();
        let trace_id = format!("{}{}", hex(&seed, u64::MAX), hex(&seed, u64::MAX - 1));
        Ok(Spans {
            state: Mutex::new(State {
                out: std::io::BufWriter::new(std::fs::File::create(path)?),
                open: HashMap::new(),
                ended: Vec::new(),
                trace_id,
                seed,
                next: 0,
            }),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The run's trace id.
    pub fn trace_id(&self) -> String {
        self.state().trace_id.clone()
    }

    /// The run's trace id is `id` (OpenTelemetry's, when it exports too).
    pub fn set_trace_id(&self, id: &str) {
        if id.len() == 32 {
            self.state().trace_id = id.to_string();
        }
    }

    /// Span `key` begins, in span `parent`.
    fn open(&self, key: u64, parent: Option<u64>, attrs: &Attributes<'_>) {
        let mut open = Open {
            span_id: String::new(),
            parent: String::new(),
            name: attrs.metadata().name().to_string(),
            client: false,
            start: now(),
            attributes: Vec::new(),
        };
        attrs.record(&mut Fields(&mut open));
        let mut s = self.state();
        s.next += 1;
        open.span_id = hex(&s.seed, s.next);
        if let Some(p) = parent.and_then(|p| s.open.get(&p)) {
            open.parent = p.span_id.clone();
        }
        s.open.insert(key, open);
    }

    /// Fields recorded on span `key` after it began.
    fn record(&self, key: u64, values: &Record<'_>) {
        if let Some(open) = self.state().open.get_mut(&key) {
            values.record(&mut Fields(open));
        }
    }

    /// Span `key` ends.
    fn close(&self, key: u64) {
        let mut s = self.state();
        let Some(o) = s.open.remove(&key) else { return };
        let span = json!({
            "traceId": s.trace_id,
            "spanId": o.span_id,
            "parentSpanId": o.parent,
            "name": o.name,
            "kind": if o.client { 3 } else { 1 },
            "startTimeUnixNano": o.start.to_string(),
            "endTimeUnixNano": now().to_string(),
            "attributes": o.attributes,
            "status": {},
        });
        s.ended.push(span);
        if s.ended.len() >= LINE {
            let _ = s.write();
        }
    }

    /// Write the spans ended so far.
    pub fn flush(&self) -> std::io::Result<()> {
        self.state().write()
    }
}

impl State {
    fn write(&mut self) -> std::io::Result<()> {
        if self.ended.is_empty() {
            return Ok(());
        }
        let spans = std::mem::take(&mut self.ended);
        let line = json!({ "resourceSpans": [{
            "resource": { "attributes": [
                { "key": "service.name", "value": { "stringValue": "dform" } },
            ] },
            "scopeSpans": [{ "scope": { "name": "dform" }, "spans": spans }],
        }] });
        writeln!(self.out, "{line}")?;
        self.out.flush()
    }
}

/// A span's fields: its name and kind, the rest attributes.
struct Fields<'a>(&'a mut Open);

impl Fields<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        self.0
            .attributes
            .push(json!({ "key": field.name(), "value": value }));
    }
}

impl Visit for Fields<'_> {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, json!({ "intValue": value.to_string() }));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, json!({ "intValue": value.to_string() }));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, json!({ "boolValue": value }));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "otel.name" => self.0.name = value.to_string(),
            "otel.kind" => self.0.client = value == "client",
            _ => self.put(field, json!({ "stringValue": value })),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

thread_local! {
    /// The spans this thread is in, innermost last.
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// The subscriber when the trace file is all there is: dform's spans
/// (target "dform"), every other dropped.
pub struct Writer {
    spans: std::sync::Arc<Spans>,
    next: AtomicU64,
    /// Each open span's metadata and how many handles it has.
    live: Mutex<HashMap<u64, (&'static Metadata<'static>, usize)>>,
}

impl Writer {
    pub fn new(spans: std::sync::Arc<Spans>) -> Writer {
        Writer {
            spans,
            next: AtomicU64::new(1),
            live: Mutex::new(HashMap::new()),
        }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<u64, (&'static Metadata<'static>, usize)>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn current(&self) -> Option<u64> {
        ENTERED.with(|e| e.borrow().last().copied())
    }
}

impl tracing::Subscriber for Writer {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        match metadata.target() == crate::timing::TARGET {
            true => Interest::always(),
            false => Interest::never(),
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == crate::timing::TARGET
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let parent = match (attrs.is_root(), attrs.parent()) {
            (true, _) => None,
            (false, Some(p)) => Some(p.into_u64()),
            (false, None) => self.current(),
        };
        self.live().insert(key, (attrs.metadata(), 1));
        self.spans.open(key, parent, attrs);
        Id::from_u64(key)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        self.spans.record(span.into_u64(), values);
    }

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, _: &Event<'_>) {}

    fn enter(&self, span: &Id) {
        ENTERED.with(|e| e.borrow_mut().push(span.into_u64()));
    }

    fn exit(&self, span: &Id) {
        ENTERED.with(|e| {
            let mut e = e.borrow_mut();
            if let Some(i) = e.iter().rposition(|k| *k == span.into_u64()) {
                e.remove(i);
            }
        });
    }

    fn clone_span(&self, id: &Id) -> Id {
        if let Some((_, n)) = self.live().get_mut(&id.into_u64()) {
            *n += 1;
        }
        id.clone()
    }

    fn try_close(&self, id: Id) -> bool {
        let key = id.into_u64();
        let closed = {
            let mut live = self.live();
            match live.get_mut(&key) {
                Some((_, n)) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    live.remove(&key);
                    true
                }
                None => false,
            }
        };
        if closed {
            self.spans.close(key);
        }
        closed
    }

    fn current_span(&self) -> tracing_core::span::Current {
        let Some(key) = self.current() else {
            return tracing_core::span::Current::none();
        };
        match self.live().get(&key) {
            Some((meta, _)) => tracing_core::span::Current::new(Id::from_u64(key), meta),
            None => tracing_core::span::Current::none(),
        }
    }
}

/// The trace file as a layer, beside the OTLP exporter.
#[cfg(feature = "otel")]
pub struct Layer(pub std::sync::Arc<Spans>);

#[cfg(feature = "otel")]
impl<S> tracing_subscriber::Layer<S> for Layer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let parent = ctx
            .span(id)
            .and_then(|s| s.parent())
            .map(|p| p.id().into_u64());
        self.0.open(id.into_u64(), parent, attrs);
    }

    fn on_record(
        &self,
        id: &Id,
        values: &Record<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        self.0.record(id.into_u64(), values);
    }

    fn on_close(&self, id: Id, _: tracing_subscriber::layer::Context<'_, S>) {
        self.0.close(id.into_u64());
    }
}
