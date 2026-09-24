// Copyright 2026 Oxide Computer Company
//! Opinionated OpenTelemetry setup for [Dropshot] servers.
//!
//! Dropshot's optional `tracing` feature makes the server create one
//! [`tracing::Span`] per request and record a documented set of fields on it
//! (the field contract is documented under "Tracing" in dropshot's crate
//! docs).
//! Dropshot itself has no opinion about what consumes those spans.  This
//! crate is one such consumer: it wires up the `tracing` machinery to export
//! the spans via OTLP, propagate W3C trace context from incoming requests,
//! export the standard HTTP server request duration metric via OTLP, and
//! (optionally) forward `tracing` events into an existing `slog` logger and
//! report per-request [`metrics`] to application code.
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
//! dropping it flushes buffered spans and metrics and shuts down the
//! exporters.
//!
//! # Configuration
//!
//! Exporting is controlled by the standard OpenTelemetry environment
//! variables, read by the OpenTelemetry SDK and OTLP exporter
//! (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
//! `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_SERVICE_NAME`,
//! `OTEL_RESOURCE_ATTRIBUTES`, and friends).  This crate reads the
//! environment but never modifies it.  Its own behaviors worth knowing:
//!
//! * Traces and metrics are exported independently.  No trace exporter is
//!   created, and spans go nowhere, if neither `OTEL_EXPORTER_OTLP_ENDPOINT`
//!   nor `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is set, if `OTEL_SDK_DISABLED`
//!   is `true`, or if `OTEL_TRACES_EXPORTER` is `none`; likewise for metrics,
//!   with `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` and `OTEL_METRICS_EXPORTER`.
//!   (The slog bridge and [`Builder::with_request_metrics`], if requested,
//!   still work.)  So a collector that accepts only traces needs
//!   `OTEL_METRICS_EXPORTER=none`.
//! * The only supported OTLP protocol is `http/protobuf`; [`Builder::install`]
//!   fails if `OTEL_EXPORTER_OTLP_PROTOCOL`,
//!   `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL`, or
//!   `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` asks for another.
//! * The exported metric is the OpenTelemetry semantic conventions'
//!   `http.server.request.duration` histogram, recorded for every request
//!   dropshot handles.  Its attributes are the conventions' standard ones
//!   (method, scheme, route, status code, error type, protocol version)
//!   except for the opt-in `server.address` and `server.port`, plus any
//!   [`metrics::label`]s.  Installing also sets the global OpenTelemetry
//!   meter provider, for the application's own metrics.
//! * Histograms, that one and any the application records, are exported
//!   with explicit (fixed) buckets by default, the request duration's being
//!   the semantic conventions' advised ones.  The standard
//!   `OTEL_EXPORTER_OTLP_METRICS_DEFAULT_HISTOGRAM_AGGREGATION` variable
//!   can select `base2_exponential_bucket_histogram` instead; an
//!   unrecognized value is ignored with a warning.  The application can
//!   choose the aggregation for particular histograms with
//!   [`Builder::with_histogram_aggregation`], which takes precedence.
//! * If neither `OTEL_SERVICE_NAME` nor a `service.name` in
//!   `OTEL_RESOURCE_ATTRIBUTES` is set, the service name passed to
//!   [`builder`] is used.
//!
//! `RUST_LOG` (via [`tracing_subscriber::EnvFilter`]) controls which spans
//! are exported and which events reach the slog bridge, defaulting to `info`
//! with noisy HTTP internals suppressed.  Dropshot's request spans are
//! INFO-level spans with target `dropshot::instrument`, so a filter that
//! excludes them (e.g. `warn`, or `myapp=debug`) also stops them being
//! exported.  It does not affect request metrics
//! ([`Builder::with_request_metrics`]).
//!
//! The exporters speak OTLP over HTTP.  With the default `tls` cargo feature
//! they can also speak HTTPS, using rustls with the aws-lc-rs provider (the
//! same TLS stack dropshot uses) and the platform certificate store; build
//! with `default-features = false` for plain-HTTP-only exporters.
//!
//! # Prometheus
//!
//! This crate exports metrics only via OTLP; it doesn't serve a Prometheus
//! scrape endpoint.  To get them into Prometheus, either:
//!
//! * Send them through an OpenTelemetry Collector, which can serve them for
//!   Prometheus to scrape or write them to Prometheus remotely.  This is the
//!   usual deployment, and the Collector can take traces too.
//! * Send them to Prometheus directly, if it runs with its OTLP receiver
//!   enabled (`--web.enable-otlp-receiver`): set
//!   `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` to
//!   `http://<prometheus>:9090/api/v1/otlp/v1/metrics`.
//!
//! Either way, by default Prometheus sees the request duration metric as
//! `http_server_request_duration_seconds`.  An application that would
//! rather be scraped directly, or already uses the [`metrics`
//! crate](https://docs.rs/metrics), can record requests itself with
//! [`Builder::with_request_metrics`]; examples/prometheus.rs does this,
//! serving the same metric, with the same labels, at `/metrics`.
//!
//! [Dropshot]: https://docs.rs/dropshot

pub mod metrics;
mod propagation;
mod slog_bridge;

pub use propagation::TraceContextLayer;
pub use slog_bridge::SlogBridge;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::{
    Aggregation, Instrument, InstrumentKind, SdkMeterProvider, Stream,
};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::{EnvResourceDetector, ResourceDetector};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
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
    Builder {
        service_name: service_name.into(),
        slog_logger: None,
        scrubber: None,
        request_metrics: None,
        histogram_aggregations: HashMap::new(),
    }
}

/// Configures and installs the global `tracing` subscriber.  See the crate
/// docs for an overview and [`builder`] to construct one.
#[derive(Debug)]
pub struct Builder {
    service_name: String,
    slog_logger: Option<slog::Logger>,
    scrubber: Option<Scrubber>,
    request_metrics: Option<RequestRecorder>,
    histogram_aggregations: HashMap<String, HistogramAggregation>,
}

/// Errors from [`Builder::install`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("failed to build an OTLP exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    #[error("a global tracing subscriber is already installed")]
    SubscriberAlreadySet(#[from] tracing::subscriber::SetGlobalDefaultError),
    #[error(
        "unsupported OTLP protocol {0:?} (only \"http/protobuf\" is \
         supported)"
    )]
    UnsupportedProtocol(String),
}

impl Builder {
    /// Also forward `tracing` events (not spans) to the given slog logger, so
    /// that instrumented libraries' log output lands in the same place as the
    /// rest of a dropshot application's logging.
    pub fn with_slog_bridge(mut self, logger: slog::Logger) -> Self {
        self.slog_logger = Some(logger);
        self
    }

    /// Applies `scrubber` to every span just before it is exported, so it
    /// can redact or remove sensitive attributes.
    ///
    /// Dropshot records request query strings as sent (in the `url.query`
    /// attribute), so they may carry secrets.  For example, to redact the
    /// values of selected query parameters (examples/otel.rs does the same):
    ///
    /// ```
    /// use opentelemetry_sdk::trace::SpanData;
    ///
    /// /// Query parameters whose values must not leave the process.
    /// const SENSITIVE_PARAMS: &[&str] = &["token", "api_key"];
    ///
    /// fn redact_query(query: &str) -> String {
    ///     query
    ///         .split('&')
    ///         .map(|param| match param.split_once('=') {
    ///             Some((name, _)) if SENSITIVE_PARAMS.contains(&name) => {
    ///                 format!("{name}=REDACTED")
    ///             }
    ///             _ => param.to_string(),
    ///         })
    ///         .collect::<Vec<_>>()
    ///         .join("&")
    /// }
    ///
    /// fn scrub_span(span: &mut SpanData) {
    ///     for attribute in &mut span.attributes {
    ///         if attribute.key.as_str() == "url.query" {
    ///             let redacted = redact_query(&attribute.value.as_str());
    ///             attribute.value = redacted.into();
    ///         }
    ///     }
    /// }
    ///
    /// let builder = dropshot_otel::builder("my-service")
    ///     .with_span_scrubber(scrub_span);
    /// # assert_eq!(
    /// #     redact_query("page=2&token=s3cret"),
    /// #     "page=2&token=REDACTED",
    /// # );
    /// ```
    ///
    /// The scrubber sees only exported spans: it has no effect when no OTLP
    /// endpoint is configured.
    pub fn with_span_scrubber(
        mut self,
        scrubber: impl Fn(&mut SpanData) + Send + Sync + 'static,
    ) -> Self {
        self.scrubber = Some(Scrubber(Arc::new(scrubber)));
        self
    }

    /// Reports every request dropshot handles to `recorder`, as it completes;
    /// see [`metrics`].  Unlike span export and the slog bridge, this does
    /// not depend on `RUST_LOG`.
    ///
    /// This is for applications that want their own view of requests; the
    /// OTLP metrics export described in the crate docs needs no recorder.
    pub fn with_request_metrics(
        mut self,
        recorder: impl Fn(&metrics::CompletedRequest) + Send + Sync + 'static,
    ) -> Self {
        self.request_metrics = Some(RequestRecorder(Box::new(recorder)));
        self
    }

    /// Exports the histogram named `instrument` (e.g. one the application
    /// records with the global meter provider) with the given aggregation,
    /// whatever the default (see the crate docs).  Instrument names match
    /// case-insensitively.
    ///
    /// For example, a histogram whose values span orders of magnitude with
    /// no natural bucket boundaries, such as payload sizes, suits
    /// [`HistogramAggregation::Exponential`].
    pub fn with_histogram_aggregation(
        mut self,
        instrument: impl Into<String>,
        aggregation: HistogramAggregation,
    ) -> Self {
        self.histogram_aggregations
            .insert(instrument.into().to_lowercase(), aggregation);
        self
    }

    /// Installs the global `tracing` subscriber and, if an OTLP endpoint is
    /// configured in the environment, the OpenTelemetry export pipelines.
    ///
    /// If there is nothing to do — neither traces nor metrics to export, and
    /// neither the slog bridge nor request metrics requested — this installs
    /// nothing and returns an inert [`Guard`], leaving the global subscriber
    /// slot free for other use.
    pub fn install(self) -> Result<Guard, InitError> {
        let export_traces = export_enabled("TRACES")?;
        let export_metrics = export_enabled("METRICS")?;
        if !export_traces
            && !export_metrics
            && self.slog_logger.is_none()
            && self.request_metrics.is_none()
        {
            return Ok(Guard { tracer_provider: None, meter_provider: None });
        }
        let resource = resource(self.service_name);

        // `RUST_LOG` filters each layer that honors it, rather than the whole
        // subscriber, so that it cannot stop request metrics.  (`EnvFilter`
        // isn't `Clone`, hence the closure.)
        let filter = || {
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
        };

        let (otel_layer, tracer_provider) = if export_traces {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()?;
            let provider = SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(ScrubbingExporter {
                    inner: exporter,
                    scrubber: self.scrubber,
                })
                .build();
            let layer = TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("dropshot-otel")),
            )
            .with_filter(filter());
            (Some(layer), Some(provider))
        } else {
            (None, None)
        };

        let bridge = self
            .slog_logger
            .map(|logger| SlogBridge::new(logger).with_filter(filter()));
        let meter_provider = if export_metrics {
            let exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .build()?;
            let view = HistogramView {
                default: default_histogram_aggregation(),
                overrides: self.histogram_aggregations,
            };
            Some(
                SdkMeterProvider::builder()
                    .with_resource(resource)
                    .with_periodic_exporter(exporter)
                    .with_view(move |instrument: &Instrument| {
                        view.stream(instrument)
                    })
                    .build(),
            )
        } else {
            None
        };
        let duration_histogram = meter_provider.as_ref().map(|provider| {
            metrics::DurationHistogram::new(&provider.meter("dropshot-otel"))
        });
        let request_metrics = match (self.request_metrics, duration_histogram) {
            (None, None) => None,
            (recorder, histogram) => Some(metrics::layer(move |request| {
                if let Some(RequestRecorder(recorder)) = &recorder {
                    recorder(request);
                }
                if let Some(histogram) = &histogram {
                    histogram.record(request);
                }
            })),
        };
        let subscriber =
            tracing_subscriber::registry().with(bridge).with(request_metrics);
        // Not `.with(otel_layer)`: `Option<Layer>` doesn't pass
        // `on_register_dispatch` through, which TraceContextLayer uses.
        match otel_layer {
            Some(layer) => {
                tracing::subscriber::set_global_default(subscriber.with(layer))?
            }
            None => tracing::subscriber::set_global_default(subscriber)?,
        }
        // Only now that the subscriber is in place, so that a failed install
        // leaves no global state behind.  The layer does its own propagation
        // of incoming trace context; the global propagator is for application
        // code propagating it onward (e.g. into outgoing requests).
        if let Some(provider) = &tracer_provider {
            opentelemetry::global::set_tracer_provider(provider.clone());
            opentelemetry::global::set_text_map_propagator(
                TraceContextPropagator::new(),
            );
        }
        // For application code's own metrics.
        if let Some(provider) = &meter_provider {
            opentelemetry::global::set_meter_provider(provider.clone());
        }
        Ok(Guard { tracer_provider, meter_provider })
    }
}

/// Keeps the OpenTelemetry export pipeline alive.  Dropping the guard flushes
/// buffered spans and metrics and shuts down the exporters, so hold it for the
/// life of the process (e.g. `let _guard = ...` in `main`).
#[derive(Debug)]
#[must_use = "dropping the Guard shuts down export"]
pub struct Guard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Guard {
    /// Synchronously flushes any buffered spans and metrics to the exporters.
    pub fn force_flush(&self) {
        if let Some(provider) = &self.tracer_provider {
            if let Err(e) = provider.force_flush() {
                eprintln!("dropshot-otel: failed to flush spans: {e}");
            }
        }
        if let Some(provider) = &self.meter_provider {
            if let Err(e) = provider.force_flush() {
                eprintln!("dropshot-otel: failed to flush metrics: {e}");
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            // Shutdown drains the batch processor's queue before returning.
            if let Err(e) = provider.shutdown() {
                eprintln!(
                    "dropshot-otel: failed to shut down tracer provider: {e}"
                );
            }
        }
        if let Some(provider) = self.meter_provider.take() {
            // Shutdown exports what has been collected since the last export.
            if let Err(e) = provider.shutdown() {
                eprintln!(
                    "dropshot-otel: failed to shut down meter provider: {e}"
                );
            }
        }
    }
}

/// A function given each completed request; see
/// [`Builder::with_request_metrics`].
struct RequestRecorder(Box<dyn Fn(&metrics::CompletedRequest) + Send + Sync>);

impl std::fmt::Debug for RequestRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestRecorder")
    }
}

/// A function applied to each span before export; see
/// [`Builder::with_span_scrubber`].
#[derive(Clone)]
struct Scrubber(Arc<dyn Fn(&mut SpanData) + Send + Sync>);

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scrubber")
    }
}

/// A span exporter that applies a [`Scrubber`] (if any) to each span before
/// passing it to the inner exporter.
#[derive(Debug)]
struct ScrubbingExporter<E> {
    inner: E,
    scrubber: Option<Scrubber>,
}

impl<E: SpanExporter> SpanExporter for ScrubbingExporter<E> {
    fn export(
        &self,
        mut batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        if let Some(Scrubber(scrub)) = &self.scrubber {
            batch.iter_mut().for_each(|span| scrub(span));
        }
        self.inner.export(batch)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// Builds the OpenTelemetry resource, using `service_name` as the
/// `service.name` attribute unless the environment provides one.
fn resource(service_name: String) -> Resource {
    // These are the two places the SDK's own resource detection looks for a
    // service name.
    let env_service_name = !env_unset("OTEL_SERVICE_NAME")
        || EnvResourceDetector::new()
            .detect()
            .get(&opentelemetry::Key::new("service.name"))
            .is_some();
    let mut resource = Resource::builder();
    if !env_service_name {
        resource = resource.with_service_name(service_name);
    }
    resource.build()
}

/// Returns whether the environment asks for the given signal (`TRACES` or
/// `METRICS`, as named in environment variables) to be exported: an OTLP
/// endpoint is configured, and neither the SDK nor the signal's export is
/// disabled.  Fails if it asks for an OTLP protocol this crate cannot speak
/// (which the exporter would otherwise silently replace with
/// "http/protobuf").
fn export_enabled(signal: &str) -> Result<bool, InitError> {
    let endpoint = !env_unset("OTEL_EXPORTER_OTLP_ENDPOINT")
        || !env_unset(&format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"));
    let sdk_disabled = std::env::var("OTEL_SDK_DISABLED")
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let exporter_none = std::env::var(format!("OTEL_{signal}_EXPORTER"))
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("none"));
    if !endpoint || sdk_disabled || exporter_none {
        return Ok(false);
    }

    let protocol = [
        format!("OTEL_EXPORTER_OTLP_{signal}_PROTOCOL"),
        "OTEL_EXPORTER_OTLP_PROTOCOL".to_string(),
    ]
    .into_iter()
    .find(|name| !env_unset(name))
    .map(|name| std::env::var(name).unwrap());
    match protocol {
        Some(protocol) if protocol != "http/protobuf" => {
            Err(InitError::UnsupportedProtocol(protocol))
        }
        _ => Ok(true),
    }
}

/// How an exported histogram aggregates its measurements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistogramAggregation {
    /// Fixed buckets: those the instrument advises (as dropshot's request
    /// duration histogram does, with the semantic conventions' boundaries),
    /// else the OpenTelemetry SDK's defaults.  Suits values whose range is
    /// known in advance.
    Explicit,
    /// Base-2 exponential buckets, whose resolution adapts to the range of
    /// values measured.  Suits values spanning orders of magnitude; needs a
    /// backend that supports exponential histograms.
    Exponential,
}

/// Returns the default histogram aggregation, per the standard
/// `OTEL_EXPORTER_OTLP_METRICS_DEFAULT_HISTOGRAM_AGGREGATION` variable
/// (which the OpenTelemetry SDK doesn't itself read).  As the specification
/// says, the default is explicit buckets, and an unrecognized value is
/// ignored with a warning.
fn default_histogram_aggregation() -> HistogramAggregation {
    const NAME: &str =
        "OTEL_EXPORTER_OTLP_METRICS_DEFAULT_HISTOGRAM_AGGREGATION";
    if env_unset(NAME) {
        return HistogramAggregation::Explicit;
    }
    let value = std::env::var(NAME).unwrap();
    match value.trim() {
        "explicit_bucket_histogram" => HistogramAggregation::Explicit,
        "base2_exponential_bucket_histogram" => {
            HistogramAggregation::Exponential
        }
        _ => {
            eprintln!(
                "dropshot-otel: ignoring unsupported {NAME} value {value:?}"
            );
            HistogramAggregation::Explicit
        }
    }
}

/// The metrics view that chooses each histogram's aggregation: the one set
/// for it by name, else the default.  (It's one view, not one per setting,
/// because the SDK aggregates an instrument once for every view that
/// matches it.)
struct HistogramView {
    default: HistogramAggregation,
    /// Aggregations by lowercased instrument name.
    overrides: HashMap<String, HistogramAggregation>,
}

impl HistogramView {
    fn stream(&self, instrument: &Instrument) -> Option<Stream> {
        if instrument.kind() != InstrumentKind::Histogram {
            return None;
        }
        let aggregation = self
            .overrides
            .get(&instrument.name().to_lowercase())
            .unwrap_or(&self.default);
        match aggregation {
            // Matching no view gives the SDK's default aggregation: explicit
            // buckets, with the instrument's advised boundaries if any.
            HistogramAggregation::Explicit => None,
            HistogramAggregation::Exponential => Some(
                Stream::builder()
                    // The specification's default size limits: at most 160
                    // buckets, at the finest scale that fits them.
                    .with_aggregation(Aggregation::Base2ExponentialHistogram {
                        max_size: 160,
                        max_scale: 20,
                        record_min_max: true,
                    })
                    .build()
                    // Building fails only for an invalid unit or cardinality
                    // limit, and this sets neither.
                    .expect("valid exponential histogram stream"),
            ),
        }
    }
}

/// Returns true if the named environment variable is unset or empty (the
/// OpenTelemetry spec treats empty as unset).
fn env_unset(name: &str) -> bool {
    std::env::var(name).map(|v| v.is_empty()).unwrap_or(true)
}

#[cfg(test)]
mod test {
    //! `install()` reads the environment and sets process-global state, so
    //! each scenario runs in a child process: a parent test re-executes this
    //! test binary to run one `child_*` test with a controlled environment.
    //! Run any other way, the `child_*` tests do nothing.

    use opentelemetry::trace::{Span as _, Tracer as _};
    use std::sync::{Arc, Mutex};

    const CHILD_ENV: &str = "DROPSHOT_OTEL_TEST_CHILD";
    const EXPECT_ENV: &str = "DROPSHOT_OTEL_TEST_EXPECT";
    /// An endpoint nothing listens on, so that any spans a test exports go
    /// nowhere.
    const ENDPOINT: (&str, &str) =
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9");

    /// Runs `test::{name}` in a child process whose environment has no
    /// `OTEL_*` or `RUST_LOG` variables except those in `env`, asserts that
    /// it ran and passed, and returns its standard error.
    fn run_child(name: &str, env: &[(&str, &str)]) -> String {
        let mut cmd =
            std::process::Command::new(std::env::current_exe().unwrap());
        cmd.arg(format!("test::{}", name)).arg("--exact").arg("--nocapture");
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key.starts_with("OTEL_") || key == "RUST_LOG" {
                cmd.env_remove(key.as_ref());
            }
        }
        cmd.env(CHILD_ENV, "1").envs(env.iter().copied());
        let output = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(" 1 passed"),
            "child test {} with {:?} failed:\n{}\n{}",
            name,
            env,
            stdout,
            stderr
        );
        stderr.into_owned()
    }

    fn is_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Returns whether spans from the global OpenTelemetry tracer are real
    /// (a tracer provider was installed) rather than no-ops.
    fn global_tracer_installed() -> bool {
        let span = opentelemetry::global::tracer("test").start("probe");
        span.span_context().is_valid()
    }

    /// Returns whether the global `tracing` subscriber slot is still free.
    fn global_subscriber_free() -> bool {
        tracing::subscriber::set_global_default(tracing_subscriber::registry())
            .is_ok()
    }

    #[test]
    fn test_install_exports() {
        run_child("child_installs", &[ENDPOINT]);
    }

    #[test]
    fn test_install_without_endpoint_is_inert() {
        run_child("child_installs_nothing", &[]);
    }

    #[test]
    fn test_install_honors_sdk_disabled() {
        run_child(
            "child_installs_nothing",
            &[ENDPOINT, ("OTEL_SDK_DISABLED", "true")],
        );
    }

    #[test]
    fn test_install_honors_traces_exporter_none() {
        // Metrics are still exported, so a subscriber is installed.
        run_child(
            "child_installs_without_tracer",
            &[ENDPOINT, ("OTEL_TRACES_EXPORTER", "none")],
        );
    }

    #[test]
    fn test_install_honors_traces_exporter_none_in_any_case() {
        run_child(
            "child_installs_without_tracer",
            &[ENDPOINT, ("OTEL_TRACES_EXPORTER", "NONE")],
        );
    }

    #[test]
    fn test_install_with_all_exporters_none() {
        run_child(
            "child_installs_nothing",
            &[
                ENDPOINT,
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none"),
            ],
        );
    }

    #[test]
    fn test_install_protocol_precedence() {
        // The signal-specific settings win over the general one.
        run_child(
            "child_installs",
            &[
                ENDPOINT,
                ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/protobuf"),
                ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "http/protobuf"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ],
        );
    }

    #[test]
    fn test_install_subscriber_already_set() {
        run_child("child_subscriber_already_set", &[ENDPOINT]);
    }

    #[test]
    fn test_install_unsupported_protocol() {
        run_child(
            "child_unsupported_protocol",
            &[ENDPOINT, ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")],
        );
        run_child(
            "child_unsupported_protocol",
            &[ENDPOINT, ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc")],
        );
        run_child(
            "child_unsupported_protocol",
            &[ENDPOINT, ("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "grpc")],
        );
    }

    #[test]
    fn child_installs() {
        if !is_child() {
            return;
        }
        let _guard = super::builder("test").install().unwrap();
        assert!(global_tracer_installed());
        assert!(!global_subscriber_free());
    }

    #[test]
    fn child_installs_without_tracer() {
        if !is_child() {
            return;
        }
        let _guard = super::builder("test").install().unwrap();
        assert!(!global_tracer_installed());
        assert!(!global_subscriber_free());
    }

    #[test]
    fn child_installs_nothing() {
        if !is_child() {
            return;
        }
        let _guard = super::builder("test").install().unwrap();
        assert!(!global_tracer_installed());
        assert!(global_subscriber_free());
    }

    #[test]
    fn child_subscriber_already_set() {
        if !is_child() {
            return;
        }
        assert!(global_subscriber_free());
        let result = super::builder("test").install();
        assert!(
            matches!(result, Err(super::InitError::SubscriberAlreadySet(_))),
            "{:?}",
            result
        );
        // The failed install must leave no OpenTelemetry globals behind.
        assert!(!global_tracer_installed());
    }

    #[test]
    fn child_unsupported_protocol() {
        if !is_child() {
            return;
        }
        let result = super::builder("test").install();
        match result {
            Err(error @ super::InitError::UnsupportedProtocol(_)) => {
                assert!(error.to_string().contains("grpc"), "{}", error);
            }
            other => panic!("expected UnsupportedProtocol: {:?}", other),
        }
        assert!(!global_tracer_installed());
        assert!(global_subscriber_free());
    }

    /// Requests received by an OTLP sink: each one's path and body.
    type Received = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// Receives OTLP/HTTP requests, recording each one's path and body.
    /// Returns the endpoint to point an exporter at, and the requests
    /// received so far.
    fn start_otlp_sink() -> (String, Received) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let received: Received = Default::default();
        let sink = Arc::clone(&received);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    let mut stream = BufReader::new(stream.unwrap());
                    // One request per iteration, until the client hangs up.
                    loop {
                        let mut request_line = String::new();
                        if stream.read_line(&mut request_line).unwrap_or(0) == 0
                        {
                            return;
                        }
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or_default()
                            .to_string();
                        let mut content_length = 0;
                        loop {
                            let mut header = String::new();
                            stream.read_line(&mut header).unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':')
                            {
                                if name.eq_ignore_ascii_case("content-length") {
                                    content_length =
                                        value.trim().parse().unwrap();
                                }
                            }
                        }
                        let mut body = vec![0; content_length];
                        stream.read_exact(&mut body).unwrap();
                        sink.lock().unwrap().push((path, body));
                        stream
                            .get_mut()
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
                            )
                            .unwrap();
                    }
                });
            }
        });
        (endpoint, received)
    }

    /// Returns whether any request `path` in `received` has a body containing
    /// `needle`.
    fn sent(
        received: &Mutex<Vec<(String, Vec<u8>)>>,
        path: &str,
        needle: &str,
    ) -> bool {
        received.lock().unwrap().iter().any(|(p, body)| {
            p == path
                && body.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
    }

    #[dropshot::endpoint {
        method = GET,
        path = "/ping",
    }]
    async fn ping(
        _rqctx: dropshot::RequestContext<()>,
    ) -> Result<dropshot::HttpResponseOk<()>, dropshot::HttpError> {
        tracing::info!("info from handler");
        tracing::warn!("warn from handler");
        Ok(dropshot::HttpResponseOk(()))
    }

    /// Starts a dropshot server, makes one request of it, and shuts it down.
    fn serve_one_request() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut api = dropshot::ApiDescription::new();
            api.register(ping).unwrap();
            let log = slog::Logger::root(slog::Discard, slog::o!());
            let server =
                dropshot::ServerBuilder::new(api, (), log).start().unwrap();
            let url = format!("http://{}/ping", server.local_addr());
            let response = reqwest::get(url).await.unwrap();
            assert_eq!(response.status(), 200);
            // Shut down so that the request span has closed.
            server.close().await.unwrap();
        });
    }

    /// A slog drain that keeps the messages it is given.
    struct Messages(Arc<Mutex<Vec<String>>>);

    impl slog::Drain for Messages {
        type Ok = ();
        type Err = slog::Never;

        fn log(
            &self,
            record: &slog::Record<'_>,
            _values: &slog::OwnedKVList,
        ) -> Result<(), slog::Never> {
            self.0.lock().unwrap().push(record.msg().to_string());
            Ok(())
        }
    }

    /// The name of the request duration metric, which appears in exported
    /// metrics.
    const DURATION_METRIC: &str = "http.server.request.duration";

    #[test]
    fn test_install_exports_request_spans_and_metrics() {
        let (endpoint, received) = start_otlp_sink();
        run_child(
            "child_serves_one_request",
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)],
        );
        // Exported, and named for the endpoint (by TraceContextLayer).
        assert!(sent(&received, "/v1/traces", "GET /ping"));
        // The route is one of the metric's attributes.
        assert!(sent(&received, "/v1/metrics", DURATION_METRIC));
        assert!(sent(&received, "/v1/metrics", "/ping"));
    }

    #[test]
    fn test_install_span_export_honors_rust_log() {
        let (endpoint, received) = start_otlp_sink();
        run_child(
            "child_serves_one_request",
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint), ("RUST_LOG", "warn")],
        );
        assert!(!sent(&received, "/v1/traces", "/ping"));
        // Metrics don't depend on RUST_LOG.
        assert!(sent(&received, "/v1/metrics", DURATION_METRIC));
    }

    #[test]
    fn test_install_exports_each_signal_independently() {
        let (endpoint, received) = start_otlp_sink();
        run_child(
            "child_serves_one_request",
            &[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
                ("OTEL_METRICS_EXPORTER", "none"),
            ],
        );
        assert!(sent(&received, "/v1/traces", "GET /ping"));
        assert!(!sent(&received, "/v1/metrics", DURATION_METRIC));

        let (endpoint, received) = start_otlp_sink();
        run_child(
            "child_serves_one_request",
            &[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
                ("OTEL_TRACES_EXPORTER", "none"),
            ],
        );
        assert!(!sent(&received, "/v1/traces", "/ping"));
        assert!(sent(&received, "/v1/metrics", DURATION_METRIC));

        // A signal-specific endpoint is used as given, and applies only to
        // that signal.
        let (endpoint, received) = start_otlp_sink();
        run_child(
            "child_serves_one_request",
            &[(
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                &format!("{}/custom/metrics", endpoint),
            )],
        );
        assert!(!sent(&received, "/v1/traces", "/ping"));
        assert!(sent(&received, "/custom/metrics", DURATION_METRIC));
    }

    #[test]
    fn child_serves_one_request() {
        if !is_child() {
            return;
        }
        let _guard = super::builder("test").install().unwrap();
        serve_one_request();
    }

    #[test]
    fn test_install_request_metrics() {
        // Request metrics don't depend on RUST_LOG, which does still filter
        // the slog bridge.
        run_child("child_request_metrics", &[("RUST_LOG", "warn")]);
    }

    #[test]
    fn child_request_metrics() {
        if !is_child() {
            return;
        }
        let messages: Arc<Mutex<Vec<String>>> = Default::default();
        let completed: Arc<Mutex<Vec<u16>>> = Default::default();
        let sink = Arc::clone(&completed);
        let _guard = super::builder("test")
            .with_slog_bridge(slog::Logger::root(
                Messages(Arc::clone(&messages)),
                slog::o!(),
            ))
            .with_request_metrics(move |request| {
                sink.lock().unwrap().push(request.status_code.unwrap());
            })
            .install()
            .unwrap();
        serve_one_request();
        assert_eq!(*completed.lock().unwrap(), [200]);
        let messages = messages.lock().unwrap();
        let from_handler: Vec<_> =
            messages.iter().filter(|m| m.ends_with("from handler")).collect();
        assert_eq!(from_handler, ["warn from handler"]);
    }

    #[test]
    fn test_install_request_metrics_alone() {
        // Request metrics alone are reason enough to install a subscriber.
        run_child("child_request_metrics_alone", &[]);
    }

    #[test]
    fn child_request_metrics_alone() {
        if !is_child() {
            return;
        }
        let completed: Arc<Mutex<Vec<u16>>> = Default::default();
        let sink = Arc::clone(&completed);
        let _guard = super::builder("test")
            .with_request_metrics(move |request| {
                sink.lock().unwrap().push(request.status_code.unwrap());
            })
            .install()
            .unwrap();
        assert!(!global_subscriber_free());
        serve_one_request();
        assert_eq!(*completed.lock().unwrap(), [200]);
    }

    #[test]
    fn test_default_histogram_aggregation() {
        const NAME: &str =
            "OTEL_EXPORTER_OTLP_METRICS_DEFAULT_HISTOGRAM_AGGREGATION";
        // As the specification says: explicit buckets unless configured
        // otherwise, and an unrecognized setting is ignored with a warning.
        for (value, expected) in [
            (None, "Explicit"),
            (Some(""), "Explicit"),
            (Some("explicit_bucket_histogram"), "Explicit"),
            (Some("base2_exponential_bucket_histogram"), "Exponential"),
            (Some("summary"), "Explicit"),
        ] {
            let mut env = vec![(EXPECT_ENV, expected)];
            env.extend(value.map(|value| (NAME, value)));
            let stderr = run_child("child_default_histogram_aggregation", &env);
            assert_eq!(
                stderr.contains("dropshot-otel: ignoring unsupported"),
                value == Some("summary"),
                "{:?}: {}",
                value,
                stderr
            );
        }
    }

    #[test]
    fn child_default_histogram_aggregation() {
        if !is_child() {
            return;
        }
        let expected = std::env::var(EXPECT_ENV).unwrap();
        assert_eq!(
            format!("{:?}", super::default_histogram_aggregation()),
            expected
        );
    }

    /// Returns the aggregation that `view` gives each of a few instruments:
    /// histograms named `a` and `B` (with advised boundaries), and a
    /// counter.
    fn aggregations(
        view: super::HistogramView,
    ) -> std::collections::BTreeMap<String, String> {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

        let exporter =
            opentelemetry_sdk::metrics::InMemoryMetricExporter::default();
        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .with_view(move |instrument: &_| view.stream(instrument))
            .build();
        let meter = provider.meter("test");
        for name in ["a", "B"] {
            let histogram = meter
                .f64_histogram(name)
                .with_boundaries(vec![1.0, 10.0])
                .build();
            histogram.record(0.5, &[]);
        }
        meter.u64_counter("requests").build().add(4, &[]);
        provider.force_flush().unwrap();

        exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .map(|metric| {
                let aggregation = match metric.data() {
                    AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                        let point = h.data_points().next().unwrap();
                        format!(
                            "explicit {:?}",
                            point.bounds().collect::<Vec<_>>()
                        )
                    }
                    AggregatedMetrics::F64(
                        MetricData::ExponentialHistogram(_),
                    ) => "exponential".to_string(),
                    AggregatedMetrics::U64(MetricData::Sum(_)) => {
                        "sum".to_string()
                    }
                    other => format!("{:?}", other),
                };
                (metric.name().to_string(), aggregation)
            })
            .collect()
    }

    #[test]
    fn test_with_histogram_aggregation() {
        use super::HistogramAggregation::{Explicit, Exponential};

        let builder = super::builder("test")
            .with_histogram_aggregation("Payload.Size", Explicit)
            // A later setting for the same instrument wins.
            .with_histogram_aggregation("payload.size", Exponential);
        assert_eq!(
            builder.histogram_aggregations,
            [("payload.size".to_string(), Exponential)].into_iter().collect()
        );
    }

    #[test]
    fn test_histogram_view() {
        use super::HistogramAggregation::{Explicit, Exponential};

        // Explicit histograms keep their advised boundaries; overrides match
        // instrument names case-insensitively; other kinds of instrument are
        // unaffected.
        let view = super::HistogramView {
            default: Explicit,
            overrides: [("b".to_string(), Exponential)].into_iter().collect(),
        };
        assert_eq!(
            aggregations(view),
            [
                ("B", "exponential"),
                ("a", "explicit [1.0, 10.0]"),
                ("requests", "sum"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into_iter()
            .collect()
        );

        let view = super::HistogramView {
            default: Exponential,
            // Keys are lowercased (by `Builder::with_histogram_aggregation`).
            overrides: [("a".to_string(), Explicit)].into_iter().collect(),
        };
        assert_eq!(
            aggregations(view),
            [
                ("B", "exponential"),
                ("a", "explicit [1.0, 10.0]"),
                ("requests", "sum"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn test_scrubbing_exporter() {
        use opentelemetry::trace::{Tracer as _, TracerProvider as _};
        use opentelemetry_sdk::trace::InMemorySpanExporter;

        let exporter = InMemorySpanExporter::default();
        let scrubber = super::Scrubber(std::sync::Arc::new(
            |span: &mut opentelemetry_sdk::trace::SpanData| {
                for attribute in &mut span.attributes {
                    if attribute.key.as_str() == "url.query" {
                        attribute.value = "REDACTED".into();
                    }
                }
            },
        ));
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(super::ScrubbingExporter {
                inner: exporter.clone(),
                scrubber: Some(scrubber),
            })
            .build();
        let tracer = provider.tracer("test");
        tracer
            .span_builder("request")
            .with_attributes([
                opentelemetry::KeyValue::new("url.query", "token=s3cret"),
                opentelemetry::KeyValue::new("url.path", "/items"),
            ])
            .start(&tracer)
            .end();

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        let attribute = |key: &str| {
            spans[0]
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.as_str().into_owned())
        };
        assert_eq!(attribute("url.query").as_deref(), Some("REDACTED"));
        assert_eq!(attribute("url.path").as_deref(), Some("/items"));
    }

    #[test]
    fn test_service_name() {
        run_child("child_service_name", &[(EXPECT_ENV, "from-builder")]);
        run_child(
            "child_service_name",
            &[(EXPECT_ENV, "from-env"), ("OTEL_SERVICE_NAME", "from-env")],
        );
        run_child(
            "child_service_name",
            &[
                (EXPECT_ENV, "from-attrs"),
                ("OTEL_RESOURCE_ATTRIBUTES", "service.name=from-attrs"),
            ],
        );
    }

    #[test]
    fn child_service_name() {
        if !is_child() {
            return;
        }
        let expected = std::env::var(EXPECT_ENV).unwrap();
        let resource = super::resource("from-builder".to_string());
        let service_name = resource
            .get(&opentelemetry::Key::new("service.name"))
            .map(|value| value.to_string());
        assert_eq!(service_name, Some(expected));
    }
}
