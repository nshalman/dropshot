// Copyright 2026 Oxide Computer Company
//! Opinionated OpenTelemetry tracing setup for [Dropshot] servers.
//!
//! Dropshot's optional `tracing` feature makes the server create one
//! [`tracing::Span`] per request and record a documented set of fields on it
//! (the field contract is documented under "Tracing" in dropshot's crate
//! docs).
//! Dropshot itself has no opinion about what consumes those spans.  This
//! crate is one such consumer: it wires up the `tracing` machinery to export
//! the spans via OTLP, propagate W3C trace context from incoming requests,
//! and (optionally) forward `tracing` events into an existing `slog` logger.
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
//! variables, read by the OpenTelemetry SDK and OTLP exporter
//! (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`,
//! `OTEL_RESOURCE_ATTRIBUTES`, and friends).  This crate reads the
//! environment but never modifies it.  Its own behaviors worth knowing:
//!
//! * No exporter is created, and spans go nowhere, if neither
//!   `OTEL_EXPORTER_OTLP_ENDPOINT` nor `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`
//!   is set, if `OTEL_SDK_DISABLED` is `true`, or if `OTEL_TRACES_EXPORTER`
//!   is `none`.  (The slog bridge, if requested, still works.)
//! * The only supported OTLP protocol is `http/protobuf`; [`Builder::install`]
//!   fails if `OTEL_EXPORTER_OTLP_PROTOCOL` or
//!   `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` asks for another.
//! * If neither `OTEL_SERVICE_NAME` nor a `service.name` in
//!   `OTEL_RESOURCE_ATTRIBUTES` is set, the service name passed to
//!   [`builder`] is used.
//!
//! `RUST_LOG` (via [`tracing_subscriber::EnvFilter`]) controls which spans
//! and events are recorded at all, defaulting to `info` with noisy HTTP
//! internals suppressed.  It filters span export as well as the slog bridge:
//! dropshot's request spans are INFO-level spans with target
//! `dropshot::instrument`, so a filter that excludes them (e.g. `warn`, or
//! `myapp=debug`) also stops them being exported.
//!
//! The exporter speaks OTLP over HTTP.  With the default `tls` cargo feature
//! it can also speak HTTPS, using rustls with the aws-lc-rs provider (the
//! same TLS stack dropshot uses) and the platform certificate store; build
//! with `default-features = false` for a plain-HTTP-only exporter.
//!
//! [Dropshot]: https://docs.rs/dropshot

mod propagation;
mod slog_bridge;

pub use propagation::TraceContextLayer;
pub use slog_bridge::SlogBridge;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::resource::{EnvResourceDetector, ResourceDetector};
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use std::sync::Arc;
use std::time::Duration;
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
    Builder {
        service_name: service_name.into(),
        slog_logger: None,
        scrubber: None,
    }
}

/// Configures and installs the global `tracing` subscriber.  See the crate
/// docs for an overview and [`builder`] to construct one.
#[derive(Debug)]
pub struct Builder {
    service_name: String,
    slog_logger: Option<slog::Logger>,
    scrubber: Option<Scrubber>,
}

/// Errors from [`Builder::install`].
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("failed to build the OTLP span exporter: {0}")]
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

    /// Installs the global `tracing` subscriber and, if an OTLP endpoint is
    /// configured in the environment, the OpenTelemetry export pipeline.
    ///
    /// If there is nothing to do — no OTLP endpoint configured and no slog
    /// bridge requested — this installs nothing and returns an inert
    /// [`Guard`], leaving the global subscriber slot free for other use.
    pub fn install(self) -> Result<Guard, InitError> {
        let export = export_enabled()?;
        if !export && self.slog_logger.is_none() {
            return Ok(Guard { provider: None });
        }

        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

        let (otel_layer, provider) = if export {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()?;
            let provider = SdkTracerProvider::builder()
                .with_resource(resource(self.service_name))
                .with_batch_exporter(ScrubbingExporter {
                    inner: exporter,
                    scrubber: self.scrubber,
                })
                .build();
            let layer = TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("dropshot-otel")),
            );
            (Some(layer), Some(provider))
        } else {
            (None, None)
        };

        let bridge = self.slog_logger.map(SlogBridge::new);
        let subscriber =
            tracing_subscriber::registry().with(filter).with(bridge);
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
        if let Some(provider) = &provider {
            opentelemetry::global::set_tracer_provider(provider.clone());
            opentelemetry::global::set_text_map_propagator(
                TraceContextPropagator::new(),
            );
        }
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

/// Returns whether the environment asks for spans to be exported: an OTLP
/// endpoint is configured, and neither the SDK nor trace export is disabled.
/// Fails if it asks for an OTLP protocol this crate cannot speak (which the
/// exporter would otherwise silently replace with "http/protobuf").
fn export_enabled() -> Result<bool, InitError> {
    let endpoint = !env_unset("OTEL_EXPORTER_OTLP_ENDPOINT")
        || !env_unset("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
    let sdk_disabled = std::env::var("OTEL_SDK_DISABLED")
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let exporter_none = std::env::var("OTEL_TRACES_EXPORTER")
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("none"));
    if !endpoint || sdk_disabled || exporter_none {
        return Ok(false);
    }

    let protocol =
        ["OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "OTEL_EXPORTER_OTLP_PROTOCOL"]
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

    const CHILD_ENV: &str = "DROPSHOT_OTEL_TEST_CHILD";
    const EXPECT_ENV: &str = "DROPSHOT_OTEL_TEST_EXPECT";
    /// An endpoint nothing listens on, so that any spans a test exports go
    /// nowhere.
    const ENDPOINT: (&str, &str) =
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9");

    /// Runs `test::{name}` in a child process whose environment has no
    /// `OTEL_*` or `RUST_LOG` variables except those in `env`, and asserts
    /// that it ran and passed.
    fn run_child(name: &str, env: &[(&str, &str)]) {
        let mut cmd =
            std::process::Command::new(std::env::current_exe().unwrap());
        cmd.arg(format!("test::{}", name)).arg("--exact");
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
        run_child(
            "child_installs_nothing",
            &[ENDPOINT, ("OTEL_TRACES_EXPORTER", "none")],
        );
    }

    #[test]
    fn test_install_honors_traces_exporter_none_in_any_case() {
        run_child(
            "child_installs_nothing",
            &[ENDPOINT, ("OTEL_TRACES_EXPORTER", "NONE")],
        );
    }

    #[test]
    fn test_install_protocol_precedence() {
        // The traces-specific setting wins over the general one.
        run_child(
            "child_installs",
            &[
                ENDPOINT,
                ("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "http/protobuf"),
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
