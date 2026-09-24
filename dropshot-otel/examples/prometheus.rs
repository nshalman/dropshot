// Copyright 2026 Oxide Computer Company
//! Example of serving request metrics for Prometheus to scrape, using the
//! [`metrics`] crate.
//!
//! dropshot-otel's own metrics export is OTLP (see examples/otel.rs), which
//! is also a way to reach Prometheus: through an OpenTelemetry Collector, or
//! Prometheus's own OTLP receiver.  This example shows the other way, for an
//! application that already uses the `metrics` crate or wants Prometheus to
//! scrape it directly.  Every request dropshot handles is reported to
//! `record_request` (via `Builder::with_request_metrics`), which records it
//! in a `metrics` histogram, and the application serves the Prometheus
//! exporter's rendering of all its metrics at `/metrics`:
//!
//! ```bash
//! cargo run --example prometheus &
//! curl http://localhost:4000/items/1
//! curl -H 'x-tenant: acme' http://localhost:4000/items/2
//! curl http://localhost:4000/nonexistent
//! curl http://localhost:4000/metrics
//! ```
//!
//! The histogram is named and labeled as OpenTelemetry's Prometheus
//! translation would name and label the standard
//! `http.server.request.duration` metric (dots become underscores, and the
//! unit becomes a suffix), with the semantic conventions' advised bucket
//! boundaries, so it looks the same whichever way it reaches Prometheus.
//!
//! [`metrics`]: https://docs.rs/metrics

use dropshot::ApiDescription;
use dropshot::Body;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingLevel;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::Path;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::endpoint;
use dropshot_otel::metrics::CompletedRequest;
use http::Response;
use http::header::CONTENT_TYPE;
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_exporter_prometheus::PrometheusHandle;
use metrics_exporter_prometheus::PrometheusRecorder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("example-prometheus")
        .map_err(|error| format!("failed to create logger: {}", error))?;

    // Install the Prometheus recorder as the `metrics` crate's global
    // recorder, keeping a handle for rendering what it has recorded.
    let recorder = prometheus_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder)
        .map_err(|error| format!("failed to install recorder: {}", error))?;
    // Without its (optional) HTTP listener, the exporter leaves periodic
    // upkeep, which keeps histograms' memory use bounded, to us.
    let upkeep_handle = handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            upkeep_handle.run_upkeep();
        }
    });

    // With no OTLP endpoint configured, this installs only the request
    // metrics layer (and the slog bridge).
    let _guard = dropshot_otel::builder("dropshot-otel-prometheus")
        .with_slog_bridge(log.clone())
        .with_request_metrics(record_request)
        .install()
        .map_err(|error| format!("failed to initialize tracing: {}", error))?;

    let mut api = ApiDescription::new();
    api.register(get_item).unwrap();
    api.register(get_metrics).unwrap();

    let server = ServerBuilder::new(api, handle, log)
        .config(dropshot::ConfigDropshot {
            bind_address: "127.0.0.1:4000".parse().unwrap(),
            ..Default::default()
        })
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;
    server.await
}

/// The histogram of request durations, as OpenTelemetry names the standard
/// `http.server.request.duration` metric for Prometheus.
const DURATION_METRIC: &str = "http_server_request_duration_seconds";

/// Returns a Prometheus recorder that records the request duration
/// histogram with the semantic conventions' advised bucket boundaries (in
/// seconds).
fn prometheus_recorder() -> PrometheusRecorder {
    PrometheusBuilder::new()
        .set_buckets(&[
            0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5,
            5.0, 7.5, 10.0,
        ])
        .expect("bucket boundaries are not empty")
        .build_recorder()
}

/// Records a completed request in the request duration histogram, labeled
/// with the semantic conventions' attributes (as for dropshot-otel's OTLP
/// metric) and any application labels, with each name's dots replaced by
/// underscores for Prometheus.
fn record_request(request: &CompletedRequest) {
    let mut labels = vec![
        ("http.request.method", request.method.clone()),
        ("url.scheme", request.url_scheme.clone()),
    ];
    let optional = [
        ("error.type", request.error_type.clone()),
        (
            "http.response.status_code",
            request.status_code.map(|code| code.to_string()),
        ),
        ("http.route", request.route.clone()),
        ("network.protocol.version", request.protocol_version.clone()),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            labels.push((key, value));
        }
    }
    let mut labels: Vec<metrics::Label> = labels
        .into_iter()
        .map(|(key, value)| metrics::Label::new(key.replace('.', "_"), value))
        .collect();
    for (key, value) in &request.labels {
        let key = key.replace('.', "_");
        // Application labels can't override the standard ones.
        if !labels.iter().any(|label| label.key() == key) {
            labels.push(metrics::Label::new(key, value.clone()));
        }
    }
    metrics::histogram!(DURATION_METRIC, labels)
        .record(request.duration.as_secs_f64());
}

#[derive(Deserialize, JsonSchema)]
struct ItemPath {
    id: u32,
}

#[derive(Serialize, JsonSchema)]
struct Item {
    id: u32,
}

/// Fetch an item.
#[endpoint {
    method = GET,
    path = "/items/{id}",
}]
async fn get_item(
    rqctx: RequestContext<PrometheusHandle>,
    path: Path<ItemPath>,
) -> Result<HttpResponseOk<Item>, HttpError> {
    // A stand-in for, say, the authenticated user's organization.  Label
    // values become separate series, so use only values from a small set.
    if let Some(tenant) = rqctx.request.headers().get("x-tenant") {
        let tenant = tenant.to_str().unwrap_or("(invalid)");
        dropshot_otel::metrics::label("tenant", tenant);
    }
    Ok(HttpResponseOk(Item { id: path.into_inner().id }))
}

/// Fetch all metrics in the Prometheus text exposition format.
#[endpoint {
    method = GET,
    path = "/metrics",
    unpublished = true,
}]
async fn get_metrics(
    rqctx: RequestContext<PrometheusHandle>,
) -> Result<Response<Body>, HttpError> {
    let rendered = rqctx.context().render();
    Ok(Response::builder()
        .header(CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(rendered.into())?)
}

#[cfg(test)]
mod test {
    use super::prometheus_recorder;
    use super::record_request;
    use dropshot_otel::metrics::CompletedRequest;
    use std::time::Duration;

    #[test]
    fn test_record_request() {
        let recorder = prometheus_recorder();
        let handle = recorder.handle();
        let mut request = CompletedRequest::default();
        request.method = "GET".to_string();
        request.url_scheme = "http".to_string();
        request.protocol_version = Some("1.1".to_string());
        request.route = Some("/items/{id}".to_string());
        request.status_code = Some(200);
        request.duration = Duration::from_millis(30);
        request.labels.insert("tenant".to_string(), "acme".to_string());
        // Application labels can't override the standard ones.
        request
            .labels
            .insert("http.route".to_string(), "/elsewhere".to_string());
        metrics::with_local_recorder(&recorder, || record_request(&request));

        let rendered = handle.render();
        // In the order `record_request` adds them.
        let labels = "http_request_method=\"GET\",\
                      url_scheme=\"http\",\
                      http_response_status_code=\"200\",\
                      http_route=\"/items/{id}\",\
                      network_protocol_version=\"1.1\",\
                      tenant=\"acme\"";
        for line in [
            format!(
                "http_server_request_duration_seconds_bucket{{{labels},le=\"0.025\"}} 0"
            ),
            format!(
                "http_server_request_duration_seconds_bucket{{{labels},le=\"0.05\"}} 1"
            ),
            format!("http_server_request_duration_seconds_count{{{labels}}} 1"),
        ] {
            assert!(
                rendered.contains(&line),
                "no {:?} in:\n{}",
                line,
                rendered
            );
        }
        assert!(!rendered.contains("/elsewhere"), "{}", rendered);
    }
}
