// Copyright 2026 Oxide Computer Company

//! Tests of the `tracing` span field contract (see the crate docs): runs a
//! real server under a `tracing` subscriber that captures every span's
//! fields, makes requests, and checks the `dropshot_request` spans.
//!
//! The capturing subscriber is installed once, as the global default, and
//! files each span under the thread that created it.  Each test uses a
//! current-thread runtime, so its server's spans are all created on the
//! test's own thread, and concurrently running tests (in this module or
//! any other in the same process) don't see each other's spans.
//!
//! (Per-test thread-local subscribers would be simpler, but under `cargo
//! test` they were flaky: request spans were intermittently never created,
//! apparently because tracing's process-wide callsite interest cache was
//! computed while other tests' threads, which had no subscriber, hit the
//! same callsite.)

use dropshot::test_util::ClientTestContext;
use dropshot::{
    ApiDescription, ApiEndpoint, ApiEndpointVersions, ConfigDropshot,
    HandlerTaskMode, HttpError, HttpResponseOk, HttpServer, Path,
    RequestContext, ServerBuilder, endpoint,
};
use http::{Method, StatusCode};
use schemars::JsonSchema;
use serde::Deserialize;
use slog::o;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::ThreadId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// A captured span field value.
#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    I64(i64),
    U64(u64),
    Bool(bool),
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::Str(value.to_string())
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::I64(value)
    }
}

#[derive(Debug, Default)]
struct Fields(BTreeMap<&'static str, Value>);

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name(), Value::Str(value.to_string()));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name(), Value::I64(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name(), Value::U64(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name(), Value::Bool(value));
    }

    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        self.0.insert(field.name(), Value::Str(format!("{:?}", value)));
    }
}

#[derive(Debug)]
struct CapturedSpan {
    id: u64,
    thread: ThreadId,
    name: &'static str,
    parent: Option<u64>,
    fields: Fields,
}

impl CapturedSpan {
    fn get(&self, field: &str) -> Option<&Value> {
        self.fields.0.get(field)
    }

    /// Asserts the value of a field, or its absence (`None`).
    fn assert_field(&self, field: &str, expected: Option<Value>) {
        assert_eq!(
            self.get(field),
            expected.as_ref(),
            "field {:?} of span {:#?}",
            field,
            self
        );
    }
}

#[derive(Default)]
struct Captured {
    /// Threads whose spans are being captured.
    threads: HashSet<ThreadId>,
    open: HashMap<u64, CapturedSpan>,
    closed: Vec<CapturedSpan>,
}

/// A layer recording the fields of every span created on a watched thread,
/// and moving the span to `closed` when it closes.
#[derive(Clone, Default)]
struct CaptureLayer(Arc<Mutex<Captured>>);

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, S>,
    ) {
        let thread = std::thread::current().id();
        if !self.0.lock().unwrap().threads.contains(&thread) {
            return;
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        let parent = ctx
            .span(id)
            .and_then(|span| span.parent())
            .map(|parent| parent.id().into_u64());
        let span = CapturedSpan {
            id: id.into_u64(),
            thread,
            name: attrs.metadata().name(),
            parent,
            fields,
        };
        self.0.lock().unwrap().open.insert(id.into_u64(), span);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        if let Some(span) = self.0.lock().unwrap().open.get_mut(&id.into_u64())
        {
            values.record(&mut span.fields);
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let mut captured = self.0.lock().unwrap();
        if let Some(span) = captured.open.remove(&id.into_u64()) {
            captured.closed.push(span);
        }
    }
}

impl CaptureLayer {
    /// Returns the process-wide capture layer, installing it (as part of the
    /// global subscriber) on first use.
    fn global() -> &'static CaptureLayer {
        static CAPTURE: OnceLock<CaptureLayer> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let layer = CaptureLayer::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(layer.clone()),
            )
            .expect("no other global tracing subscriber");
            layer
        })
    }

    /// Starts capturing spans created on the current thread.
    fn watch_current_thread(&self) {
        self.0.lock().unwrap().threads.insert(std::thread::current().id());
    }

    /// Stops capturing spans created on the current thread, and returns
    /// those that have closed: the request spans, and the spans of any
    /// other name.
    fn finish_current_thread(&self) -> (Vec<CapturedSpan>, Vec<CapturedSpan>) {
        let thread = std::thread::current().id();
        let mut captured = self.0.lock().unwrap();
        captured.threads.remove(&thread);
        let (mine, others): (Vec<_>, Vec<_>) =
            std::mem::take(&mut captured.closed)
                .into_iter()
                .partition(|span| span.thread == thread);
        captured.closed = others;
        mine.into_iter().partition(|span| span.name == "dropshot_request")
    }
}

/// Server context: lets a test wait until the `/hang` handler has started.
#[derive(Default)]
struct SpanTestContext {
    hang_started: Notify,
}

#[derive(Deserialize, JsonSchema)]
struct ItemPath {
    id: u32,
}

#[endpoint {
    method = GET,
    path = "/items/{id}",
}]
async fn get_item(
    _rqctx: RequestContext<Arc<SpanTestContext>>,
    path: Path<ItemPath>,
) -> Result<HttpResponseOk<u32>, HttpError> {
    // A span created by the handler must be a child of the request span.
    let _child = tracing::info_span!("handler_child").entered();
    Ok(HttpResponseOk(path.into_inner().id))
}

#[endpoint {
    method = GET,
    path = "/bad",
}]
async fn get_bad(
    _rqctx: RequestContext<Arc<SpanTestContext>>,
) -> Result<HttpResponseOk<u32>, HttpError> {
    Err(HttpError::for_bad_request(None, "bad thing".to_string()))
}

#[endpoint {
    method = GET,
    path = "/broken",
}]
async fn get_broken(
    _rqctx: RequestContext<Arc<SpanTestContext>>,
) -> Result<HttpResponseOk<u32>, HttpError> {
    Err(HttpError::for_internal_error("it broke".to_string()))
}

#[endpoint {
    method = GET,
    path = "/hang",
}]
async fn get_hang(
    rqctx: RequestContext<Arc<SpanTestContext>>,
) -> Result<HttpResponseOk<u32>, HttpError> {
    rqctx.context().hang_started.notify_one();
    std::future::pending().await
}

/// A handler for a method outside the standard list, registered directly
/// (the `endpoint` macro only accepts standard methods).
async fn propfind_dav(
    _rqctx: RequestContext<Arc<SpanTestContext>>,
) -> Result<HttpResponseOk<u32>, HttpError> {
    Ok(HttpResponseOk(0))
}

struct TestServer {
    server: HttpServer<Arc<SpanTestContext>>,
    logctx: dropshot::test_util::LogContext,
}

impl TestServer {
    fn start(name: &str, handler_task_mode: HandlerTaskMode) -> Self {
        CaptureLayer::global().watch_current_thread();
        let logctx = crate::common::create_log_context(name);
        let mut api = ApiDescription::new();
        api.register(get_item).unwrap();
        api.register(get_bad).unwrap();
        api.register(get_broken).unwrap();
        api.register(get_hang).unwrap();
        api.register(ApiEndpoint::new(
            "propfind_dav".to_string(),
            propfind_dav,
            Method::from_bytes(b"PROPFIND").unwrap(),
            "application/json",
            "/dav",
            ApiEndpointVersions::All,
        ))
        .unwrap();
        let server =
            ServerBuilder::new(api, Arc::default(), logctx.log.new(o!()))
                .config(ConfigDropshot {
                    default_handler_task_mode: handler_task_mode,
                    ..Default::default()
                })
                .start()
                .unwrap();
        TestServer { server, logctx }
    }

    fn client(&self) -> ClientTestContext {
        ClientTestContext::new(
            self.server.local_addr(),
            self.logctx.log.new(o!()),
        )
    }

    /// Sends `request` verbatim on a new connection and returns the raw
    /// response.  The request should ask for the connection to be closed.
    async fn raw_request(&self, request: &str) -> String {
        let mut stream =
            tokio::net::TcpStream::connect(self.server.local_addr())
                .await
                .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    /// Shuts the server down (so that every request span has closed) and
    /// returns the request spans, and all other spans.
    async fn finish(self) -> (Vec<CapturedSpan>, Vec<CapturedSpan>) {
        self.server.close().await.unwrap();
        let spans = CaptureLayer::global().finish_current_thread();
        self.logctx.cleanup_successful();
        spans
    }
}

fn str(value: &str) -> Option<Value> {
    Some(Value::from(value))
}

fn int(value: i64) -> Option<Value> {
    Some(Value::from(value))
}

const TRACEPARENT: &str =
    "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

#[tokio::test]
async fn test_request_span_success() {
    let testctx =
        TestServer::start("request_span_success", HandlerTaskMode::Detached);
    let port = i64::from(testctx.server.local_addr().port());
    testctx
        .raw_request(&format!(
            "GET /items/7?color=blue HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             User-Agent: span-test/1.0\r\n\
             traceparent: {TRACEPARENT}\r\n\
             tracestate: vendor=value\r\n\
             Connection: close\r\n\r\n"
        ))
        .await;
    let (spans, others) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];

    span.assert_field("otel.kind", str("server"));
    span.assert_field("otel.name", str("GET /items/{id}"));
    span.assert_field("http.request.method", str("GET"));
    span.assert_field("http.request.method_original", None);
    span.assert_field("url.path", str("/items/7"));
    span.assert_field("url.query", str("color=blue"));
    span.assert_field("url.scheme", str("http"));
    span.assert_field("network.protocol.version", str("1.1"));
    span.assert_field("server.address", str("127.0.0.1"));
    span.assert_field("server.port", int(port));
    span.assert_field("client.address", str("127.0.0.1"));
    span.assert_field("network.peer.address", str("127.0.0.1"));
    assert!(matches!(span.get("network.peer.port"), Some(Value::I64(_))));
    span.assert_field("client.port", None);
    span.assert_field("user_agent.original", str("span-test/1.0"));
    span.assert_field("http.request.header.traceparent", str(TRACEPARENT));
    span.assert_field("http.request.header.tracestate", str("vendor=value"));
    assert!(matches!(span.get("dropshot.request_id"), Some(Value::Str(_))));
    span.assert_field("http.route", str("/items/{id}"));
    span.assert_field("dropshot.operation_id", str("get_item"));
    span.assert_field("http.response.status_code", int(200));
    for field in [
        "error.type",
        "otel.status_code",
        "otel.status_description",
        "dropshot.error.message",
        "dropshot.error.message_external",
    ] {
        span.assert_field(field, None);
    }
    // Fields the contract replaced must not linger.
    for field in ["http.request.uri", "http.request.id", "traceparent", "error"]
    {
        span.assert_field(field, None);
    }

    // The handler's span is a child of the request span.
    let child = others
        .iter()
        .find(|s| s.name == "handler_child")
        .expect("no handler span");
    assert_eq!(child.parent, Some(span.id));
}

#[tokio::test]
async fn test_request_span_minimal_request() {
    let testctx =
        TestServer::start("request_span_minimal", HandlerTaskMode::Detached);
    // HTTP/1.0 without a Host header, query string, or other headers.
    testctx.raw_request("GET /items/7 HTTP/1.0\r\n\r\n").await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("network.protocol.version", str("1.0"));
    span.assert_field("url.path", str("/items/7"));
    for field in [
        "url.query",
        "server.address",
        "server.port",
        "user_agent.original",
        "http.request.header.traceparent",
        "http.request.header.tracestate",
    ] {
        span.assert_field(field, None);
    }
}

#[tokio::test]
async fn test_request_span_server_address() {
    let testctx = TestServer::start(
        "request_span_server_address",
        HandlerTaskMode::Detached,
    );
    // Without a port, the port is the scheme's default.
    testctx
        .raw_request(
            "GET /items/1 HTTP/1.1\r\nHost: example.com\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    // IPv6 literals are reported without their brackets.
    testctx
        .raw_request(
            "GET /items/2 HTTP/1.1\r\nHost: [::1]:8080\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 2, "{:#?}", spans);
    let span = |path| {
        spans.iter().find(|s| s.get("url.path") == str(path).as_ref()).unwrap()
    };
    span("/items/1").assert_field("server.address", str("example.com"));
    span("/items/1").assert_field("server.port", int(80));
    span("/items/2").assert_field("server.address", str("::1"));
    span("/items/2").assert_field("server.port", int(8080));
}

#[tokio::test]
async fn test_request_span_client_error() {
    let testctx = TestServer::start(
        "request_span_client_error",
        HandlerTaskMode::Detached,
    );
    testctx
        .client()
        .make_request_error(Method::GET, "/bad", StatusCode::BAD_REQUEST)
        .await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("http.response.status_code", int(400));
    // 4xx responses are not errors on server spans.
    span.assert_field("error.type", None);
    span.assert_field("otel.status_code", None);
    span.assert_field("otel.status_description", None);
    span.assert_field("dropshot.error.message", str("bad thing"));
    span.assert_field("dropshot.error.message_external", str("bad thing"));
}

#[tokio::test]
async fn test_request_span_server_error() {
    let testctx = TestServer::start(
        "request_span_server_error",
        HandlerTaskMode::Detached,
    );
    testctx
        .client()
        .make_request_error(
            Method::GET,
            "/broken",
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("http.response.status_code", int(500));
    span.assert_field("error.type", str("500"));
    span.assert_field("otel.status_code", str("ERROR"));
    span.assert_field("otel.status_description", str("it broke"));
    span.assert_field("dropshot.error.message", str("it broke"));
    span.assert_field(
        "dropshot.error.message_external",
        str("Internal Server Error"),
    );
}

#[tokio::test]
async fn test_request_span_unrouted() {
    let testctx =
        TestServer::start("request_span_unrouted", HandlerTaskMode::Detached);
    testctx
        .client()
        .make_request_error(Method::GET, "/nowhere", StatusCode::NOT_FOUND)
        .await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    // No route: the span is named for the method alone.
    span.assert_field("otel.name", str("GET"));
    span.assert_field("http.route", None);
    span.assert_field("dropshot.operation_id", None);
    span.assert_field("http.response.status_code", int(404));
}

#[tokio::test]
async fn test_request_span_unknown_method() {
    let testctx = TestServer::start(
        "request_span_unknown_method",
        HandlerTaskMode::Detached,
    );
    testctx
        .raw_request(
            "FROB /items/1 HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
        )
        .await;
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("http.request.method", str("_OTHER"));
    span.assert_field("http.request.method_original", str("FROB"));
    span.assert_field("otel.name", str("HTTP"));
    span.assert_field("http.route", None);
}

#[tokio::test]
async fn test_request_span_method_case() {
    let testctx = TestServer::start(
        "request_span_method_case",
        HandlerTaskMode::Detached,
    );
    // Dropshot routes methods case-insensitively, so this reaches get_item,
    // and the span reports the canonical method.
    let response = testctx
        .raw_request(
            "get /items/1 HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response);
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("http.request.method", str("GET"));
    span.assert_field("http.request.method_original", str("get"));
    span.assert_field("otel.name", str("GET /items/{id}"));
}

#[tokio::test]
async fn test_request_span_routed_custom_method() {
    let testctx = TestServer::start(
        "request_span_routed_custom_method",
        HandlerTaskMode::Detached,
    );
    // A method some endpoint handles counts as known, even if it's not a
    // standard one.
    let response = testctx
        .raw_request(
            "PROPFIND /dav HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response);
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    span.assert_field("http.request.method", str("PROPFIND"));
    span.assert_field("http.request.method_original", None);
    span.assert_field("otel.name", str("PROPFIND /dav"));
}

#[tokio::test]
async fn test_request_span_client_disconnect() {
    let testctx = TestServer::start(
        "request_span_client_disconnect",
        HandlerTaskMode::CancelOnDisconnect,
    );
    let mut stream =
        tokio::net::TcpStream::connect(testctx.server.local_addr())
            .await
            .unwrap();
    stream.write_all(b"GET /hang HTTP/1.1\r\nHost: h\r\n\r\n").await.unwrap();
    testctx.server.app_private().hang_started.notified().await;
    drop(stream);
    let (spans, _) = testctx.finish().await;
    assert_eq!(spans.len(), 1, "{:#?}", spans);
    let span = &spans[0];
    // No response was sent, so there is no status code to report.
    span.assert_field("http.response.status_code", None);
    span.assert_field("error.type", str("client_disconnect"));
    span.assert_field("otel.status_code", str("ERROR"));
    span.assert_field(
        "otel.status_description",
        str("client disconnected before response returned"),
    );
}
