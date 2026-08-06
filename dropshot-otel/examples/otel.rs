// Copyright 2026 Oxide Computer Company
//! Example use of Dropshot with OpenTelemetry tracing.
//!
//! Run an OTLP-over-HTTP collector (e.g. an otel-enabled Jaeger
//! all-in-one) and point the exporter at it:
//!
//! ```bash
//! export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
//! cargo run --example otel &
//! curl http://localhost:4000/counter
//! curl -H 'traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01' \
//!     http://localhost:4000/counter
//! ```
//!
//! Each request appears as a trace; the second joins the trace given in its
//! `traceparent` header.  Without `OTEL_EXPORTER_OTLP_ENDPOINT` set, the
//! server runs normally and exports nothing.

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

    // Install the OpenTelemetry export pipeline (if configured in the
    // environment) and forward tracing events to our slog logger.  The
    // guard flushes and shuts down the exporter when dropped.
    let _guard = dropshot_otel::builder("dropshot-otel-example")
        .with_slog_bridge(log.clone())
        .install()
        .map_err(|error| format!("failed to initialize tracing: {}", error))?;

    let mut api = ApiDescription::new();
    api.register(example_api_get_counter).unwrap();
    api.register(example_api_put_counter).unwrap();
    api.register(example_api_error).unwrap();

    let server = ServerBuilder::new(api, ExampleContext::default(), log)
        .config(dropshot::ConfigDropshot {
            bind_address: "127.0.0.1:4000".parse().unwrap(),
            ..Default::default()
        })
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;

    server.await
}

/// Application-specific example context (state shared by handler functions)
#[derive(Default)]
struct ExampleContext {
    counter: AtomicU64,
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
    tracing::info!(monotonic_counter.example_get = 1, "fetching counter");
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
