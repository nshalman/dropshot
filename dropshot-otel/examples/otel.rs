// Copyright 2026 Oxide Computer Company
//! Example use of Dropshot with OpenTelemetry tracing and metrics.
//!
//! Run an OTLP-over-HTTP collector (e.g. an otel-enabled Jaeger
//! all-in-one) and point the exporter at it:
//!
//! ```bash
//! export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
//! cargo run --example otel &
//! curl http://localhost:4000/counter
//! TP=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01
//! curl -H "traceparent: $TP" http://localhost:4000/counter
//! ```
//!
//! Each request appears as a trace; the second joins the trace given in its
//! `traceparent` header.
//!
//! Metrics are exported once a minute and on exit (set
//! `OTEL_METRICS_EXPORTER=none` if the collector doesn't accept metrics).
//! Two histograms show the two ways OpenTelemetry can bucket values:
//!
//! * Request durations (the standard `http.server.request.duration` metric)
//!   use the default: fixed buckets, here the boundaries the semantic
//!   conventions advise for request durations, so the metric compares
//!   directly with other HTTP servers'.
//! * The values the counter is set to (`example.counter.value`, recorded by
//!   the application itself) use exponential buckets: clients can set any
//!   value from 0 to 2^64 - 1, so there are no sensible fixed boundaries,
//!   and exponential buckets keep their relative resolution at any scale.
//!
//! ```bash
//! for n in 3 1000 250000 70000000000; do
//!     curl -X PUT -H 'content-type: application/json' \
//!         -d "{\"counter\": $n}" http://localhost:4000/counter
//! done
//! ```
//!
//! Query parameters named in `SENSITIVE_PARAMS` are redacted before export:
//! after
//!
//! ```bash
//! curl 'http://localhost:4000/counter?token=s3cret&verbose=1'
//! ```
//!
//! the span's `url.query` reads `token=REDACTED&verbose=1`.
//!
//! Without `OTEL_EXPORTER_OTLP_ENDPOINT` set, the server runs normally and
//! exports nothing.  Stop the server with Ctrl-C (SIGINT) so that buffered
//! spans and metrics are flushed on the way out.

use dropshot::ApiDescription;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingLevel;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::TypedBody;
use dropshot::endpoint;
use dropshot_otel::HistogramAggregation;
use opentelemetry::metrics::Histogram;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("example-otel")
        .map_err(|error| format!("failed to create logger: {}", error))?;

    // Install the OpenTelemetry export pipelines (if configured in the
    // environment) and forward tracing events to our slog logger.  The
    // guard flushes and shuts down the exporters when dropped.
    let _guard = dropshot_otel::builder("dropshot-otel-example")
        .with_slog_bridge(log.clone())
        .with_span_scrubber(scrub_span)
        .with_histogram_aggregation(
            COUNTER_VALUE_METRIC,
            HistogramAggregation::Exponential,
        )
        .install()
        .map_err(|error| format!("failed to initialize tracing: {}", error))?;

    // Instruments come from the global meter provider, which `install()`
    // set up (if metrics export is configured; otherwise they do nothing).
    let context = ExampleContext {
        counter: AtomicU64::new(0),
        counter_values: opentelemetry::global::meter("dropshot-otel-example")
            .u64_histogram(COUNTER_VALUE_METRIC)
            .with_description("Values the counter was set to.")
            .build(),
    };

    let mut api = ApiDescription::new();
    api.register(example_api_get_counter).unwrap();
    api.register(example_api_put_counter).unwrap();
    api.register(example_api_error).unwrap();

    let server = ServerBuilder::new(api, context, log)
        .config(dropshot::ConfigDropshot {
            bind_address: "127.0.0.1:4000".parse().unwrap(),
            ..Default::default()
        })
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;

    // Run until the server fails or we're interrupted.  Either way, return
    // (rather than dying by signal) so that `_guard` is dropped and flushes
    // buffered spans and metrics.
    tokio::select! {
        result = server => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

/// Query parameters whose values must not leave the process.
const SENSITIVE_PARAMS: &[&str] = &["token", "api_key"];

/// Replaces the values of sensitive query parameters with `REDACTED`.
fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((name, _)) if SENSITIVE_PARAMS.contains(&name) => {
                format!("{name}=REDACTED")
            }
            _ => param.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Redacts sensitive query parameters from the `url.query` attribute that
/// dropshot records on request spans.
fn scrub_span(span: &mut opentelemetry_sdk::trace::SpanData) {
    for attribute in &mut span.attributes {
        if attribute.key.as_str() == "url.query" {
            let redacted = redact_query(&attribute.value.as_str());
            attribute.value = redacted.into();
        }
    }
}

/// The name of the histogram of values the counter is set to.
const COUNTER_VALUE_METRIC: &str = "example.counter.value";

/// Application-specific example context (state shared by handler functions)
struct ExampleContext {
    counter: AtomicU64,
    counter_values: Histogram<u64>,
}

/// `CounterValue` represents the value of the API's counter, either as the
/// response to a GET request to fetch the counter or as the body of a PUT
/// request to update the counter.
#[derive(Deserialize, Serialize, JsonSchema)]
struct CounterValue {
    counter: u64,
}

/// Fetch the current value of the counter.
#[endpoint {
    method = GET,
    path = "/counter",
}]
async fn example_api_get_counter(
    rqctx: RequestContext<ExampleContext>,
) -> Result<HttpResponseOk<CounterValue>, HttpError> {
    let api_context = rqctx.context();
    // Handler code can add its own spans and events, which appear as
    // children of dropshot's per-request span.
    tracing::info!(route = "/counter", "fetching counter");
    Ok(HttpResponseOk(CounterValue {
        counter: api_context.counter.load(Ordering::SeqCst),
    }))
}

/// Update the current value of the counter.  Note that the special value of 10
/// is not allowed (just to demonstrate how to generate an error).
#[endpoint {
    method = PUT,
    path = "/counter",
}]
async fn example_api_put_counter(
    rqctx: RequestContext<ExampleContext>,
    update: TypedBody<CounterValue>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let api_context = rqctx.context();
    let updated_value = update.into_inner();

    if updated_value.counter == 10 {
        Err(HttpError::for_bad_request(
            Some(String::from("BadInput")),
            format!("do not like the number {}", updated_value.counter),
        ))
    } else {
        api_context.counter.store(updated_value.counter, Ordering::SeqCst);
        api_context.counter_values.record(updated_value.counter, &[]);
        Ok(HttpResponseUpdatedNoContent())
    }
}

/// Always fails, to demonstrate how error responses look on request spans.
#[endpoint {
    method = GET,
    path = "/error",
}]
async fn example_api_error(
    _rqctx: RequestContext<ExampleContext>,
) -> Result<HttpResponseOk<CounterValue>, HttpError> {
    Err(HttpError::for_internal_error("something bad happened".to_string()))
}
