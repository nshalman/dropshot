// Copyright 2026 Oxide Computer Company
//! Opinionated OpenTelemetry tracing setup for [Dropshot] servers.
//!
//! Dropshot's optional `tracing` feature makes the server create one
//! [`tracing::Span`] per request and record a documented set of fields on it
//! (see the docs of dropshot's internal `instrument` module for the field
//! contract).  Dropshot
//! itself has no opinion about what consumes those spans.  This crate is one
//! such consumer: it wires up the `tracing` machinery to export the spans via
//! OTLP, propagate W3C trace context from incoming requests, and (optionally)
//! forward `tracing` events into an existing `slog` logger.
//!
//! # Usage
//!
//! Build your server with dropshot's `tracing` feature enabled, then, early
//! in `main` (before other code installs a global `tracing` subscriber):
//!
//! ```no_run
//! # fn example(log: slog::Logger) -> Result<(), dropshot_otel::InitError> {
//! let _guard = dropshot_otel::builder("my-service")
//!     .with_slog_bridge(log.clone())
//!     .install()?;
//! # Ok(())
//! # }
//! ```
//!
//! Keep the returned [`Guard`] alive for the lifetime of the process;
//! dropping it flushes buffered spans and shuts down the exporter.
//!
//! # Configuration
//!
//! Exporting is controlled by the standard OpenTelemetry environment
//! variables, read by the OTLP exporter itself (`OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
//! `OTEL_SERVICE_NAME`, and friends).  This crate reads the environment but
//! never modifies it.  Two of its own behaviors are worth knowing:
//!
//! * If neither `OTEL_EXPORTER_OTLP_ENDPOINT` nor
//!   `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is set, no exporter is created and
//!   spans go nowhere (the slog bridge, if requested, still works).
//! * If `OTEL_SERVICE_NAME` is not set, the service name passed to
//!   [`builder`] is used instead.
//!
//! Event and span verbosity is controlled by `RUST_LOG` (via
//! [`tracing_subscriber::EnvFilter`]), defaulting to `info` with noisy HTTP
//! internals suppressed.
//!
//! The exporter speaks OTLP over HTTP.  With the default `tls` cargo feature
//! it can also speak HTTPS, using rustls with the aws-lc-rs provider (the
//! same TLS stack dropshot uses) and the platform certificate store; see the
//! Cargo features for alternatives.
//!
//! [Dropshot]: https://docs.rs/dropshot

mod propagation;
mod slog_bridge;

pub use propagation::TraceContextLayer;
pub use slog_bridge::SlogBridge;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;

/// Default `EnvFilter` directive when `RUST_LOG` is not set: our own spans
/// and events at "info", with the HTTP stack's internal chatter suppressed.
const DEFAULT_FILTER: &str =
    "info,h2=warn,hyper=warn,reqwest=warn,rustls=warn,tower=warn";

/// Returns a [`Builder`] for installing this crate's tracing subscriber.
///
/// `service_name` becomes the OpenTelemetry `service.name` resource attribute
/// unless the `OTEL_SERVICE_NAME` environment variable overrides it.
pub fn builder(service_name: impl Into<String>) -> Builder {
    Builder { service_name: service_name.into(), slog_logger: None }
}

/// Configures and installs the global `tracing` subscriber.  See the crate
/// docs for an overview and [`builder`] to construct one.
#[derive(Debug)]
pub struct Builder {
    service_name: String,
    slog_logger: Option<slog::Logger>,
}

/// Errors from [`Builder::install`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("failed to build the OTLP span exporter")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    #[error("a global tracing subscriber is already installed")]
    SubscriberAlreadySet(#[from] tracing::subscriber::SetGlobalDefaultError),
}

impl Builder {
    /// Also forward `tracing` events (not spans) to the given slog logger, so
    /// that instrumented libraries' log output lands in the same place as the
    /// rest of a dropshot application's logging.
    pub fn with_slog_bridge(mut self, logger: slog::Logger) -> Self {
        self.slog_logger = Some(logger);
        self
    }

    /// Installs the global `tracing` subscriber and, if an OTLP endpoint is
    /// configured in the environment, the OpenTelemetry export pipeline.
    ///
    /// If there is nothing to do — no OTLP endpoint configured and no slog
    /// bridge requested — this installs nothing and returns an inert
    /// [`Guard`], leaving the global subscriber slot free for other use.
    pub fn install(self) -> Result<Guard, InitError> {
        let export = otlp_endpoint_configured();
        if !export && self.slog_logger.is_none() {
            return Ok(Guard { provider: None });
        }

        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

        let (otel_layer, provider) = if export {
            let mut resource = Resource::builder();
            if env_unset("OTEL_SERVICE_NAME") {
                resource = resource.with_service_name(self.service_name);
            }
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()?;
            let provider = SdkTracerProvider::builder()
                .with_resource(resource.build())
                .with_batch_exporter(exporter)
                .build();
            opentelemetry::global::set_tracer_provider(provider.clone());
            opentelemetry::global::set_text_map_propagator(
                TraceContextPropagator::new(),
            );
            let layer = TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("dropshot-otel")),
            );
            (Some(layer), Some(provider))
        } else {
            (None, None)
        };

        let bridge = self.slog_logger.map(SlogBridge::new);
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(otel_layer)
            .with(bridge);
        tracing::subscriber::set_global_default(subscriber)?;
        Ok(Guard { provider })
    }
}

/// Keeps the OpenTelemetry export pipeline alive.  Dropping the guard flushes
/// buffered spans and shuts down the exporter, so hold it for the life of the
/// process (e.g. `let _guard = ...` in `main`).
#[derive(Debug)]
#[must_use = "dropping the Guard shuts down span export"]
pub struct Guard {
    provider: Option<SdkTracerProvider>,
}

impl Guard {
    /// Synchronously flushes any buffered spans to the exporter.
    pub fn force_flush(&self) {
        if let Some(provider) = &self.provider {
            if let Err(e) = provider.force_flush() {
                eprintln!("dropshot-otel: failed to flush spans: {e}");
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Shutdown drains the batch processor's queue before returning.
            if let Err(e) = provider.shutdown() {
                eprintln!(
                    "dropshot-otel: failed to shut down tracer provider: {e}"
                );
            }
        }
    }
}

fn otlp_endpoint_configured() -> bool {
    !env_unset("OTEL_EXPORTER_OTLP_ENDPOINT")
        || !env_unset("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
}

/// Returns true if the named environment variable is unset or empty (the
/// OpenTelemetry spec treats empty as unset).
fn env_unset(name: &str) -> bool {
    std::env::var(name).map(|v| v.is_empty()).unwrap_or(true)
}
