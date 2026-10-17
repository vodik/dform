//! Where a run's spans go (`dform_core::timing`): to a file as OTLP JSON
//! lines (`--trace FILE`, `DFORM_TRACE=FILE`), which Jaeger and
//! OpenTelemetry's tools read, in every build ([`file`]); over OTLP to a
//! collector when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, in a build with
//! the `otel` feature (the exporter's own variables say the rest; without
//! the feature it is ignored); and as `DFORM_LOG`'s lines on stderr
//! (`dform_core::log`). Each is a layer of one subscriber, the registry;
//! every other library's spans are dropped. The run reads nothing back: a
//! viewer does. With none asked for, no subscriber is installed and a
//! span costs its callsite's check.
//!
//! A run is one trace: its spans are under one `dform.run` span, named
//! for the command; a run that fails says its trace id.

use anyhow::{Context, Result};
use dform_core::timing::TARGET;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry};

mod file;

/// A layer of the run's subscriber.
type Boxed = Box<dyn Layer<Registry> + Send + Sync>;

/// The trace file, flushed at the run's end.
static FILE: OnceLock<Arc<file::Spans>> = OnceLock::new();

/// The trace file asked for: `--trace`, else `DFORM_TRACE`.
fn asked(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| {
        std::env::var_os("DFORM_TRACE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
}

/// The trace file's layer, when one is asked for.
fn file_layer(flag: Option<PathBuf>) -> Result<Option<Boxed>> {
    let Some(f) = asked(flag) else {
        return Ok(None);
    };
    let spans = Arc::new(
        file::Spans::create(&f).with_context(|| format!("the trace file {}", f.display()))?,
    );
    let _ = FILE.set(spans.clone());
    Ok(Some(file::Layer(spans).boxed()))
}

/// The run's spans exported where the run asks, and `DFORM_LOG`'s lines
/// said: the returned guard holds the run's span, `dform COMMAND`, until
/// it is dropped. `None` when neither is asked for.
pub fn install(flag: Option<PathBuf>, command: &str) -> Result<Option<Run>> {
    let traces: Vec<Boxed> = [file_layer(flag)?, endpoint::layer()?]
        .into_iter()
        .flatten()
        .collect();
    let traced = !traces.is_empty();
    let layers: Vec<Boxed> = traces.into_iter().chain(dform_core::log::layer()).collect();
    if layers.is_empty() {
        return Ok(None);
    }
    let subscriber =
        tracing_subscriber::registry()
            .with(layers)
            .with(tracing_subscriber::filter::filter_fn(|m| {
                m.target() == TARGET
            }));
    let _ = tracing::subscriber::set_global_default(subscriber);
    let span = tracing::debug_span!(
        target: TARGET,
        "dform.run",
        otel.name = %format_args!("dform {command}"),
        command,
    );
    // OpenTelemetry's id when it exports too, so the file and the
    // collector name the run alike; else the file's own.
    let trace_id = traced.then(|| {
        let id = endpoint::trace_id(&span)
            .or_else(|| FILE.get().map(|f| f.trace_id()))
            .unwrap_or_default();
        if let Some(f) = FILE.get() {
            f.set_trace_id(&id);
        }
        id
    });
    Ok(Some(Run {
        _span: span.entered(),
        trace_id,
    }))
}

/// The run's span, open until dropped.
pub struct Run {
    _span: tracing::span::EnteredSpan,
    trace_id: Option<String>,
}

impl Run {
    /// The run's trace id, as a viewer finds it; `None` when only
    /// `DFORM_LOG` is asked for.
    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }
}

/// The run is over, its span ended (`Run` dropped): what the exporters
/// hold is written and sent.
pub fn finish() {
    endpoint::finish();
    if let Some(f) = FILE.get()
        && let Err(e) = f.flush()
    {
        eprintln!("warning: the trace file was not all written: {e}");
    }
}

#[cfg(not(feature = "otel"))]
mod endpoint {
    use anyhow::Result;

    pub fn layer() -> Result<Option<super::Boxed>> {
        Ok(None)
    }

    pub fn trace_id(_: &tracing::Span) -> Option<String> {
        None
    }

    pub fn finish() {}
}

/// OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, a layer of the run's
/// subscriber beside the trace file. Its spans are sent in batches as
/// they end, the run's own last: at the run's end what is queued is sent
/// at the collector's pace, as long as it takes them ([`Export::flush`]),
/// and what it did not take is said.
#[cfg(feature = "otel")]
mod endpoint {
    use anyhow::{Context, Result};
    use opentelemetry::trace::{TraceContextExt, TracerProvider};
    use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
    use opentelemetry_otlp::WithHttpConfig;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
    use opentelemetry_sdk::trace::{
        BatchSpanProcessor, SdkTracerProvider, SpanData, SpanExporter, SpanProcessor,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::Layer;

    /// A request's timeout: OTLP's default for an export
    /// (`OTEL_EXPORTER_OTLP_TIMEOUT`), which bounds when the exporter
    /// starts a retry.
    const REQUEST: Duration = Duration::from_secs(10);

    /// How long the run's end waits on a collector that takes no span:
    /// an export's retries, and the last one's request.
    const STALL: Duration = REQUEST.saturating_mul(2);

    /// The exporter, flushed at the run's end.
    static EXPORTER: OnceLock<Export> = OnceLock::new();

    /// The batch processor the spans are exported through, and what of
    /// them reached the collector.
    struct Export {
        batch: Arc<BatchSpanProcessor>,
        tally: Arc<Tally>,
    }

    /// The spans the run ended and those the collector took; the exports
    /// that failed, and the last one's error.
    #[derive(Debug, Default)]
    struct Tally {
        ended: AtomicUsize,
        sent: AtomicUsize,
        failed: AtomicUsize,
        error: Mutex<Option<String>>,
    }

    /// The exporter's layer, when the endpoint is set.
    pub fn layer() -> Result<Option<super::Boxed>> {
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none_or(|v| v.is_empty()) {
            return Ok(None);
        }
        let mut otlp = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(OneClient)
            .build()
            .context("the OTLP exporter (OTEL_EXPORTER_OTLP_ENDPOINT)")?;
        otlp.set_resource(&Resource::builder().with_service_name("dform").build());
        let tally = Arc::new(Tally::default());
        let batch = Arc::new(
            BatchSpanProcessor::builder(Tallied {
                otlp,
                tally: tally.clone(),
            })
            .build(),
        );
        let provider = SdkTracerProvider::builder()
            .with_span_processor(Counted {
                batch: batch.clone(),
                tally: tally.clone(),
            })
            .build();
        let tracer = provider.tracer("dform");
        let _ = EXPORTER.set(Export { batch, tally });
        Ok(Some(
            tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_location(false)
                .with_threads(false)
                .with_target(false)
                .boxed(),
        ))
    }

    /// The trace id OpenTelemetry gave the run's span.
    pub fn trace_id(span: &tracing::Span) -> Option<String> {
        EXPORTER.get()?;
        Some(span.context().span().span_context().trace_id().to_string())
    }

    /// The run's spans sent ([`Export::finish`]).
    pub fn finish() {
        if let Some(e) = EXPORTER.get() {
            e.finish();
        }
    }

    impl Export {
        /// Every span the run ended sent, then the exporter shut down (its
        /// result unread: the tally knows what was lost); what the
        /// collector did not take said.
        fn finish(&self) {
            if self.flush() {
                let _ = self.batch.shutdown();
            }
            if let Some(lost) = self.tally.lost() {
                eprintln!("warning: the trace was not all sent: {lost}");
            }
        }

        /// Send what is queued. The batch processor's flush waits 5s
        /// whatever it holds, its exports going on past it, so it is asked
        /// again while the collector takes them: a slow one is sent the
        /// run's last batches, the run's span among them, at its pace.
        /// `false`, an export still going, once one fails (the rest would
        /// too) or none is taken for [`STALL`].
        fn flush(&self) -> bool {
            let failed = self.tally.failed();
            let mut taken = (self.tally.sent(), Instant::now());
            loop {
                if !matches!(self.batch.force_flush(), Err(OTelSdkError::Timeout(_))) {
                    // Drained: sent, or its failure tallied.
                    return true;
                }
                if self.tally.failed() != failed {
                    return false;
                }
                let sent = self.tally.sent();
                if sent != taken.0 {
                    taken = (sent, Instant::now());
                } else if taken.1.elapsed() >= STALL {
                    return false;
                }
            }
        }
    }

    impl Tally {
        fn sent(&self) -> usize {
            self.sent.load(Ordering::Acquire)
        }

        fn failed(&self) -> usize {
            self.failed.load(Ordering::Acquire)
        }

        /// The spans the collector did not take, how many and why.
        fn lost(&self) -> Option<String> {
            let ended = self.ended.load(Ordering::Acquire);
            let lost = ended.saturating_sub(self.sent());
            let why = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
            (lost > 0).then(|| {
                format!(
                    "{lost} of its {ended} spans did not reach the collector: {}",
                    why.unwrap_or_else(|| format!("it took none in {}s", STALL.as_secs()))
                )
            })
        }

        /// An export of `n` spans settled with `r`.
        fn export(&self, n: usize, r: &OTelSdkResult) {
            match r {
                Ok(()) => {
                    self.sent.fetch_add(n, Ordering::AcqRel);
                }
                Err(e) => {
                    *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                    self.failed.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
    }

    /// The batch processor as the provider's, each span counted as it
    /// ends (one its queue had no room for included: it is lost, not
    /// sent).
    #[derive(Debug)]
    struct Counted {
        batch: Arc<BatchSpanProcessor>,
        tally: Arc<Tally>,
    }

    impl SpanProcessor for Counted {
        fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
            self.batch.on_start(span, cx);
        }

        fn on_end(&self, span: SpanData) {
            self.tally.ended.fetch_add(1, Ordering::AcqRel);
            self.batch.on_end(span);
        }

        fn force_flush(&self) -> OTelSdkResult {
            self.batch.force_flush()
        }

        fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
            self.batch.shutdown_with_timeout(timeout)
        }

        /// The exporter was given the run's resource as it was built.
        fn set_resource(&mut self, _: &Resource) {}
    }

    /// The OTLP exporter, each export's spans tallied as it settles.
    #[derive(Debug)]
    struct Tallied {
        otlp: opentelemetry_otlp::SpanExporter,
        tally: Arc<Tally>,
    }

    impl SpanExporter for Tallied {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            let n = batch.len();
            let r = self.otlp.export(batch).await;
            self.tally.export(n, &r);
            r
        }

        fn set_resource(&mut self, resource: &Resource) {
            self.otlp.set_resource(resource);
        }
    }

    /// OTLP over the one HTTP client (R-103, `dform_core::http`): the
    /// machine's CA and proxy, the pool's connection to the collector, its
    /// own (the providers' windows are theirs, in the host). A request
    /// blocks the exporter's thread, never the run's.
    #[derive(Debug)]
    struct OneClient;

    #[async_trait::async_trait]
    impl HttpClient for OneClient {
        async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            let (parts, body) = request.into_parts();
            let req = dform_core::plugin::host::HttpRequest {
                method: parts.method.to_string(),
                url: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into()))
                    .collect(),
                body: body.to_vec(),
                timeout: Some(REQUEST),
                ..Default::default()
            };
            let r = dform_core::http::send(req, None, None).map_err(|e| e.message)?;
            let mut resp = Response::builder().status(r.status);
            for (k, v) in &r.headers {
                resp = resp.header(k, v);
            }
            Ok(resp.body(Bytes::from(r.body))?)
        }
    }
}
