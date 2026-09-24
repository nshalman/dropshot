// Copyright 2026 Oxide Computer Company
//! End-to-end test of request metrics: runs real dropshot servers (with the
//! `tracing` feature) under a subscriber with the request metrics layer,
//! makes requests, and checks what the layer reports for each.
//!
//! This is a single #[tokio::test] because it installs the global `tracing`
//! subscriber, of which a process gets exactly one.

use dropshot::ApiDescription;
use dropshot::ConfigDropshot;
use dropshot::HandlerTaskMode;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::endpoint;
use dropshot_otel::metrics::CompletedRequest;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use tracing_subscriber::layer::SubscriberExt;

/// A layer that does nothing, but wants every span, as (say) a span exporter
/// would: without one, the metrics layer's own filter would leave handlers'
/// spans uncreated.
struct AllSpans;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AllSpans {}

#[derive(Default)]
struct Context {
    hang_started: Notify,
}

#[endpoint {
    method = GET,
    path = "/ping",
}]
async fn ping(
    _rqctx: RequestContext<Context>,
) -> Result<HttpResponseOk<String>, HttpError> {
    Ok(HttpResponseOk("pong".to_string()))
}

#[endpoint {
    method = GET,
    path = "/fail",
}]
async fn fail(
    _rqctx: RequestContext<Context>,
) -> Result<HttpResponseOk<String>, HttpError> {
    Err(HttpError::for_bad_request(None, "nope".to_string()))
}

#[endpoint {
    method = GET,
    path = "/broken",
}]
async fn broken(
    _rqctx: RequestContext<Context>,
) -> Result<HttpResponseOk<String>, HttpError> {
    Err(HttpError::for_internal_error("it broke".to_string()))
}

#[endpoint {
    method = GET,
    path = "/tenants/{tenant}",
}]
async fn tenant(
    _rqctx: RequestContext<Context>,
    path: dropshot::Path<TenantPath>,
) -> Result<HttpResponseOk<String>, HttpError> {
    let tenant = path.into_inner().tenant;
    // Labels attach to the request even from within a handler's own span.
    let span = tracing::info_span!("lookup");
    assert!(!span.is_disabled());
    let _entered = span.enter();
    dropshot_otel::metrics::label("tenant", tenant.clone());
    // A later label with the same key replaces the earlier one.
    dropshot_otel::metrics::label("plan", "free");
    dropshot_otel::metrics::label("plan", "pro");
    Ok(HttpResponseOk(tenant))
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct TenantPath {
    tenant: String,
}

#[endpoint {
    method = GET,
    path = "/hang",
}]
async fn hang(
    rqctx: RequestContext<Context>,
) -> Result<HttpResponseOk<String>, HttpError> {
    rqctx.context().hang_started.notify_one();
    std::future::pending().await
}

fn start_server(mode: HandlerTaskMode) -> dropshot::HttpServer<Context> {
    let log = slog::Logger::root(slog::Discard, slog::o!());
    let mut api = ApiDescription::new();
    api.register(ping).unwrap();
    api.register(fail).unwrap();
    api.register(broken).unwrap();
    api.register(tenant).unwrap();
    api.register(hang).unwrap();
    ServerBuilder::new(api, Context::default(), log)
        .config(ConfigDropshot {
            default_handler_task_mode: mode,
            ..Default::default()
        })
        .start()
        .unwrap()
}

#[tokio::test]
async fn test_request_metrics() {
    let completed: Arc<Mutex<Vec<CompletedRequest>>> = Default::default();
    let sink = Arc::clone(&completed);
    let subscriber = tracing_subscriber::registry()
        .with(dropshot_otel::metrics::layer(
            move |request: &CompletedRequest| {
                sink.lock().unwrap().push(request.clone());
            },
        ))
        .with(AllSpans);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    // Outside of any request, labeling does nothing (and doesn't panic).
    dropshot_otel::metrics::label("stray", "ignored");

    let server = start_server(HandlerTaskMode::Detached);
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    for (path, status) in [
        ("/ping", 200),
        ("/fail", 400),
        ("/broken", 500),
        ("/nonexistent", 404),
        ("/tenants/acme", 200),
    ] {
        let response =
            client.get(format!("{}{}", base, path)).send().await.unwrap();
        assert_eq!(response.status(), status, "GET {}", path);
    }
    // Shut the server down so that all request spans have closed.
    server.close().await.unwrap();

    // A client disconnect, which needs a handler that is cancelled with its
    // request.
    let server = start_server(HandlerTaskMode::CancelOnDisconnect);
    let mut stream =
        tokio::net::TcpStream::connect(server.local_addr()).await.unwrap();
    stream.write_all(b"GET /hang HTTP/1.1\r\nHost: h\r\n\r\n").await.unwrap();
    server.app_private().hang_started.notified().await;
    drop(stream);
    server.close().await.unwrap();

    let completed = completed.lock().unwrap();
    assert_eq!(completed.len(), 6, "{:#?}", completed);
    let find = |path: &str| {
        completed.iter().find(|r| r.url_path == path).unwrap_or_else(|| {
            panic!("no metrics for {}: {:#?}", path, completed)
        })
    };

    let ok = find("/ping");
    assert_eq!(ok.method, "GET");
    assert_eq!(ok.url_scheme, "http");
    assert_eq!(ok.protocol_version.as_deref(), Some("1.1"));
    assert_eq!(ok.route.as_deref(), Some("/ping"));
    assert_eq!(ok.operation_id.as_deref(), Some("ping"));
    assert_eq!(ok.status_code, Some(200));
    assert_eq!(ok.error_type, None);
    assert!(ok.duration > Duration::ZERO);
    assert!(ok.labels.is_empty());

    let client_error = find("/fail");
    assert_eq!(client_error.operation_id.as_deref(), Some("fail"));
    assert_eq!(client_error.status_code, Some(400));
    assert_eq!(client_error.error_type, None);

    let server_error = find("/broken");
    assert_eq!(server_error.operation_id.as_deref(), Some("broken"));
    assert_eq!(server_error.status_code, Some(500));
    assert_eq!(server_error.error_type.as_deref(), Some("500"));

    // Requests that never reach a handler are counted too.
    let unrouted = find("/nonexistent");
    assert_eq!(unrouted.route, None);
    assert_eq!(unrouted.operation_id, None);
    assert_eq!(unrouted.status_code, Some(404));

    let labeled = find("/tenants/acme");
    assert_eq!(labeled.route.as_deref(), Some("/tenants/{tenant}"));
    assert_eq!(
        labeled.labels,
        [
            ("plan".to_string(), "pro".to_string()),
            ("tenant".to_string(), "acme".to_string())
        ]
        .into_iter()
        .collect()
    );

    let disconnected = find("/hang");
    assert_eq!(disconnected.operation_id.as_deref(), Some("hang"));
    assert_eq!(disconnected.status_code, None);
    assert_eq!(disconnected.error_type.as_deref(), Some("client_disconnect"));
}
