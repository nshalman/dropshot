// Copyright 2026 Oxide Computer Company
//! End-to-end test of the dropshot span field contract and this crate's
//! OpenTelemetry glue: runs a real dropshot server (with its `tracing`
//! feature) under a subscriber wired to an in-memory span exporter, makes
//! requests, and checks the exported spans.
//!
//! This is a single #[tokio::test] because it installs the global `tracing`
//! subscriber, of which a process gets exactly one.

use dropshot::ApiDescription;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::endpoint;
use opentelemetry::Value;
use opentelemetry::trace::{SpanId, SpanKind, TraceId, TracerProvider as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt;

#[endpoint {
    method = GET,
    path = "/ping",
}]
async fn ping(
    _rqctx: RequestContext<()>,
) -> Result<HttpResponseOk<String>, HttpError> {
    Ok(HttpResponseOk("pong".to_string()))
}

#[endpoint {
    method = GET,
    path = "/fail",
}]
async fn fail(
    _rqctx: RequestContext<()>,
) -> Result<HttpResponseOk<String>, HttpError> {
    Err(HttpError::for_bad_request(None, "nope".to_string()))
}

const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
const PARENT_SPAN_ID: &str = "b7ad6b7169203331";

fn attr<'a>(
    span: &'a opentelemetry_sdk::trace::SpanData,
    key: &str,
) -> Option<&'a Value> {
    span.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| &kv.value)
}

#[tokio::test]
async fn test_request_spans_are_exported() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_text_map_propagator(
        TraceContextPropagator::new(),
    );
    let subscriber = tracing_subscriber::registry().with(
        dropshot_otel::TraceContextLayer::new(
            tracing_opentelemetry::layer().with_tracer(provider.tracer("test")),
        ),
    );
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let log = slog::Logger::root(slog::Discard, slog::o!());
    let mut api = ApiDescription::new();
    api.register(ping).unwrap();
    api.register(fail).unwrap();
    let server = ServerBuilder::new(api, (), log).start().unwrap();
    let base = format!("http://{}", server.local_addr());

    let client = reqwest::Client::new();

    // A request carrying W3C trace context.
    let response = client
        .get(format!("{}/ping", base))
        .header("traceparent", format!("00-{}-{}-01", TRACE_ID, PARENT_SPAN_ID))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // A request with no trace context.
    let response = client.get(format!("{}/ping", base)).send().await.unwrap();
    assert_eq!(response.status(), 200);

    // A request that produces an error response.
    let response = client.get(format!("{}/fail", base)).send().await.unwrap();
    assert_eq!(response.status(), 400);

    // Shut the server down so all request spans have closed.
    server.close().await.unwrap();
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 3, "expected one span per request: {:#?}", spans);
    for span in &spans {
        assert_eq!(span.span_kind, SpanKind::Server);
        assert!(attr(span, "http.request.id").is_some());
    }

    // The traceparent request: span must be parented into the remote trace
    // and named from the resolved endpoint.
    let traced = spans
        .iter()
        .find(|s| {
            s.span_context.trace_id() == TraceId::from_hex(TRACE_ID).unwrap()
        })
        .expect("no span joined the propagated trace");
    assert_eq!(
        traced.parent_span_id,
        SpanId::from_hex(PARENT_SPAN_ID).unwrap()
    );
    assert_eq!(traced.name, "GET ping");
    assert_eq!(
        attr(traced, "dropshot.operation_id"),
        Some(&Value::from("ping"))
    );
    assert_eq!(
        attr(traced, "http.response.status_code"),
        Some(&Value::from(200))
    );

    // The context-free request: same endpoint, but a root span of a new
    // trace.
    let root = spans
        .iter()
        .find(|s| s.name == "GET ping" && !std::ptr::eq(*s, traced))
        .expect("no root span for the second request");
    assert_ne!(root.span_context.trace_id(), traced.span_context.trace_id());
    assert_eq!(root.parent_span_id, SpanId::INVALID);

    // The error request: error fields and status recorded.
    let failed = spans
        .iter()
        .find(|s| s.name == "GET fail")
        .expect("no span for the failing request");
    assert_eq!(
        attr(failed, "http.response.status_code"),
        Some(&Value::from(400))
    );
    assert_eq!(attr(failed, "error"), Some(&Value::from(true)));
    // for_bad_request uses the same message internally and externally.
    assert_eq!(attr(failed, "error.message"), Some(&Value::from("nope")));
    assert_eq!(
        attr(failed, "error.message.external"),
        Some(&Value::from("nope"))
    );
    assert_eq!(
        attr(failed, "dropshot.operation_id"),
        Some(&Value::from("fail"))
    );
}
