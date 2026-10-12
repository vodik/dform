//! Where a run's spans go (`dform_core::timing`): to a file as OTLP JSON
//! lines (`--trace FILE`, `DFORM_TRACE=FILE`), which Jaeger and
//! OpenTelemetry's tools read, in every build ([`file`]); and over OTLP
//! to a collector when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, in a build
//! with the `otel` feature (the exporter's own variables say the rest;
//! without the feature it is ignored). The run reads nothing back: a
//! viewer does. Every other library's spans are dropped. With neither
//! asked for, no subscriber is installed and a span costs its callsite's
//! check.
//!
//! A run is one trace: its spans are under one `dform.run` span, named
//! for the command; a run that fails says its trace id.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

mod file;

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

/// The run's spans exported where the run asks: the returned guard holds
/// the run's span, `dform COMMAND`, until it is dropped. `None` when no
/// trace is asked for.
pub fn install(flag: Option<PathBuf>, command: &str) -> Result<Option<Run>> {
    let spans = match asked(flag) {
        Some(f) => {
            Some(Arc::new(file::Spans::create(&f).with_context(|| {
                format!("the trace file {}", f.display())
            })?))
        }
        None => None,
    };
    if let Some(s) = &spans {
        let _ = FILE.set(s.clone());
    }
    let traced = endpoint::install(spans.clone())?
        || spans
            .is_some_and(|s| tracing::subscriber::set_global_default(file::Writer::new(s)).is_ok());
    if !traced {
        return Ok(None);
    }
    let span = tracing::info_span!(
        target: crate::timing::TARGET,
        "dform.run",
        otel.name = %format_args!("dform {command}"),
        command,
    );
    crate::timing::set_root(span.id());
    // OpenTelemetry's id when it exports too, so the file and the
    // collector name the run alike; else the file's own.
    let trace_id = endpoint::trace_id(&span)
        .or_else(|| FILE.get().map(|f| f.trace_id()))
        .unwrap_or_default();
    if let Some(f) = FILE.get() {
        f.set_trace_id(&trace_id);
    }
    Ok(Some(Run {
        _span: span.entered(),
        trace_id,
    }))
}

/// The run's span, open until dropped.
pub struct Run {
    _span: tracing::span::EnteredSpan,
    trace_id: String,
}

impl Run {
    /// The run's trace id, as a viewer finds it.
    pub fn trace_id(&self) -> &str {
        &self.trace_id
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
    use std::sync::Arc;

    pub fn install(_: Option<Arc<super::file::Spans>>) -> Result<bool> {
        Ok(false)
    }

    pub fn trace_id(_: &tracing::Span) -> Option<String> {
        None
    }

    pub fn finish() {}
}

/// OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, the trace file beside it as
/// a layer of the same subscriber.
#[cfg(feature = "otel")]
mod endpoint {
    use crate::timing::TARGET;
    use anyhow::{Context, Result};
    use opentelemetry::trace::{TraceContextExt, TracerProvider};
    use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
    use opentelemetry_otlp::{SpanExporter, WithHttpConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::prelude::*;

    /// The provider the spans are exported through, shut down (flushed)
    /// at the run's end.
    static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

    /// The exporter, when the endpoint is set; whether it is.
    pub fn install(file: Option<Arc<super::file::Spans>>) -> Result<bool> {
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none_or(|v| v.is_empty()) {
            return Ok(false);
        }
        let otlp = SpanExporter::builder()
            .with_http()
            .with_http_client(Ureq::new())
            .build()
            .context("the OTLP exporter (OTEL_EXPORTER_OTLP_ENDPOINT)")?;
        let provider = SdkTracerProvider::builder()
            .with_resource(Resource::builder().with_service_name("dform").build())
            .with_batch_exporter(otlp)
            .build();
        let tracer = provider.tracer("dform");
        let _ = PROVIDER.set(provider);
        let _ = tracing_subscriber::registry()
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_location(false)
                    .with_threads(false)
                    .with_target(false),
            )
            .with(file.map(super::file::Layer))
            .with(filter_fn(|m| m.target() == TARGET))
            .try_init();
        Ok(true)
    }

    /// The trace id OpenTelemetry gave the run's span.
    pub fn trace_id(span: &tracing::Span) -> Option<String> {
        PROVIDER.get()?;
        Some(span.context().span().span_context().trace_id().to_string())
    }

    pub fn finish() {
        if let Some(p) = PROVIDER.get()
            && let Err(e) = p.shutdown()
        {
            eprintln!("warning: the trace was not all sent: {e}");
        }
    }

    /// OTLP over the one HTTP client (R-103).
    #[derive(Debug)]
    struct Ureq(ureq::Agent);

    impl Ureq {
        fn new() -> Ureq {
            let config = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(10)))
                .build();
            Ureq(config.into())
        }
    }

    #[async_trait::async_trait]
    impl HttpClient for Ureq {
        async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            let (parts, body) = request.into_parts();
            let request = Request::from_parts(parts, body.to_vec());
            let (parts, mut body) = self.0.run(request)?.into_parts();
            Ok(Response::from_parts(
                parts,
                Bytes::from(body.read_to_vec()?),
            ))
        }
    }
}
