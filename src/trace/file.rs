//! The trace file, in every build: each span as OTLP's JSON encoding
//! has it, the spans ended so far written as one line, an
//! `ExportTraceServiceRequest` (`resourceSpans`, `scopeSpans`, `spans`),
//! as OpenTelemetry's file exporter writes them and Jaeger loads them.
//! Written here from the span data, with no OpenTelemetry crate: a span's
//! name is its `otel.name`, its kind its `otel.kind` (`client` for a
//! call, `server` for a component's export of one, else internal), its
//! status its `otel.status_code` (a call's answer, `ok` or `error`), every
//! other field an attribute.
//!
//! [`Layer`] feeds it, a layer of the run's subscriber beside the
//! exporter and `DFORM_LOG`'s lines. A call (a client span) ends when it
//! closes, when its answer is taken, though the work done for it entered
//! it (a component's export, the requests it sends); another span when it
//! is last exited, if it was ever entered (a phase, its function's body),
//! else when it closes: a span the registry keeps open for a child that
//! outlives it ends where its work did.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};

/// The spans ended this many at a time make a line.
const LINE: usize = 512;

/// OTLP's span kind of a call.
const CLIENT: u8 = 3;

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
    /// OTLP's span kind: 1 internal, 2 server, 3 client.
    kind: u8,
    start: u128,
    /// When it was last exited, if it was ever entered and is not a call.
    exited: Option<u128>,
    /// Its status, from `otel.status_code`: OTLP's code (1 ok, 2 error).
    status: Option<u8>,
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
            kind: 1,
            start: now(),
            exited: None,
            status: None,
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

    /// Span `key` is exited: a call's ends when it closes.
    fn exit(&self, key: u64) {
        if let Some(open) = self.state().open.get_mut(&key)
            && open.kind != CLIENT
        {
            open.exited = Some(now());
        }
    }

    /// Span `key` closes: it ended when it was last exited, else now.
    fn close(&self, key: u64) {
        let mut s = self.state();
        let Some(o) = s.open.remove(&key) else { return };
        let end = o.exited.unwrap_or_else(now);
        let span = json!({
            "traceId": s.trace_id,
            "spanId": o.span_id,
            "parentSpanId": o.parent,
            "name": o.name,
            "kind": o.kind,
            "startTimeUnixNano": o.start.to_string(),
            "endTimeUnixNano": end.to_string(),
            "attributes": o.attributes,
            "status": match o.status {
                Some(code) => json!({ "code": code }),
                None => json!({}),
            },
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
            "otel.kind" => {
                self.0.kind = match value {
                    "server" => 2,
                    "client" => CLIENT,
                    _ => 1,
                }
            }
            "otel.status_code" => {
                self.0.status = match value {
                    "ok" => Some(1),
                    _ => Some(2),
                }
            }
            _ => self.put(field, json!({ "stringValue": value })),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

/// The trace file as a layer of the run's subscriber.
pub struct Layer(pub std::sync::Arc<Spans>);

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

    fn on_exit(&self, id: &Id, _: tracing_subscriber::layer::Context<'_, S>) {
        self.0.exit(id.into_u64());
    }

    fn on_close(&self, id: Id, _: tracing_subscriber::layer::Context<'_, S>) {
        self.0.close(id.into_u64());
    }
}
