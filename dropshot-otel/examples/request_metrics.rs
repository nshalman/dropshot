// Copyright 2026 Oxide Computer Company
//! Example of per-request metrics for a Dropshot server, derived from
//! dropshot's request spans with no per-endpoint instrumentation code.
//!
//! This example keeps a latency histogram per (endpoint, status code,
//! labels) in memory and serves it at `/metrics`:
//!
//! ```bash
//! cargo run --example request_metrics &
//! curl http://localhost:4000/items/1
//! curl -H 'x-tenant: acme' http://localhost:4000/items/2
//! curl http://localhost:4000/items/0
//! curl http://localhost:4000/nonexistent
//! curl -s http://localhost:4000/metrics
//! ```
//!
//! Requests that never reach a handler (like the 404 above) are counted too,
//! as are requests whose client disconnected before the response.  The
//! `x-tenant` header shows application-specific labels: the handler attaches
//! the tenant to its request with `dropshot_otel::metrics::label`.

use dropshot::ApiDescription;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingLevel;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::Path;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use dropshot::endpoint;
use dropshot_otel::metrics::CompletedRequest;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_logging =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = config_logging
        .to_logger("example-request-metrics")
        .map_err(|error| format!("failed to create logger: {}", error))?;

    let metrics = Arc::new(Metrics::default());
    let recorder = Arc::clone(&metrics);
    // With no OTLP endpoint configured, this installs only the request
    // metrics layer (and the slog bridge).
    let _guard = dropshot_otel::builder("dropshot-otel-request-metrics")
        .with_slog_bridge(log.clone())
        .with_request_metrics(move |request| recorder.record(request))
        .install()
        .map_err(|error| format!("failed to initialize tracing: {}", error))?;

    let mut api = ApiDescription::new();
    api.register(get_item).unwrap();
    api.register(get_metrics).unwrap();

    let server = ServerBuilder::new(api, metrics, log)
        .config(dropshot::ConfigDropshot {
            bind_address: "127.0.0.1:4000".parse().unwrap(),
            ..Default::default()
        })
        .start()
        .map_err(|error| format!("failed to create server: {}", error))?;
    server.await
}

/// Upper bounds of the latency histogram's buckets, in microseconds.  The
/// last bucket has no upper bound.
const BUCKET_BOUNDS_US: &[u64] = &[
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000,
    250_000, 500_000, 1_000_000,
];

/// Latency histograms for all requests, one per distinct [`SeriesKey`].
#[derive(Default)]
struct Metrics {
    series: Mutex<BTreeMap<SeriesKey, Histogram>>,
}

/// What distinguishes one series of requests from another.
#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct SeriesKey {
    /// The endpoint, or `None` if the request matched none.
    operation_id: Option<String>,
    /// The response's status code, or `None` if the client disconnected
    /// first.
    status_code: Option<u16>,
    labels: BTreeMap<String, String>,
}

struct Histogram {
    count: u64,
    total_us: u64,
    /// Counts of requests per bucket; see `BUCKET_BOUNDS_US`.
    buckets: Vec<u64>,
}

impl Metrics {
    fn record(&self, request: &CompletedRequest) {
        let key = SeriesKey {
            operation_id: request.operation_id.clone(),
            status_code: request.status_code,
            labels: request.labels.clone(),
        };
        let latency_us =
            u64::try_from(request.duration.as_micros()).unwrap_or(u64::MAX);
        let mut series = self.series.lock().unwrap();
        let histogram = series.entry(key).or_insert_with(|| Histogram {
            count: 0,
            total_us: 0,
            buckets: vec![0; BUCKET_BOUNDS_US.len() + 1],
        });
        histogram.count += 1;
        histogram.total_us = histogram.total_us.saturating_add(latency_us);
        let bucket = BUCKET_BOUNDS_US
            .iter()
            .position(|bound| latency_us <= *bound)
            .unwrap_or(BUCKET_BOUNDS_US.len());
        histogram.buckets[bucket] += 1;
    }
}

#[derive(Deserialize, JsonSchema)]
struct ItemPath {
    id: u32,
}

#[derive(Serialize, JsonSchema)]
struct Item {
    id: u32,
}

/// Fetch an item.  Item 0 does not exist.
#[endpoint {
    method = GET,
    path = "/items/{id}",
}]
async fn get_item(
    rqctx: RequestContext<Arc<Metrics>>,
    path: Path<ItemPath>,
) -> Result<HttpResponseOk<Item>, HttpError> {
    // A stand-in for, say, the authenticated user's organization.  Label
    // values become separate series, so use only values from a small set.
    if let Some(tenant) = rqctx.request.headers().get("x-tenant") {
        let tenant = tenant.to_str().unwrap_or("(invalid)");
        dropshot_otel::metrics::label("tenant", tenant);
    }
    match path.into_inner().id {
        0 => Err(HttpError::for_not_found(None, "no item 0".to_string())),
        id => Ok(HttpResponseOk(Item { id })),
    }
}

/// One series in the `/metrics` response.
#[derive(Serialize, JsonSchema)]
struct Series {
    operation_id: Option<String>,
    status_code: Option<u16>,
    labels: BTreeMap<String, String>,
    count: u64,
    total_us: u64,
    buckets: Vec<Bucket>,
}

/// The number of requests in one latency bucket.
#[derive(Serialize, JsonSchema)]
struct Bucket {
    /// The bucket's upper bound in microseconds (none for the last).
    le_us: Option<u64>,
    count: u64,
}

/// Fetch the request latency histograms.
#[endpoint {
    method = GET,
    path = "/metrics",
}]
async fn get_metrics(
    rqctx: RequestContext<Arc<Metrics>>,
) -> Result<HttpResponseOk<Vec<Series>>, HttpError> {
    let series = rqctx.context().series.lock().unwrap();
    let series = series
        .iter()
        .map(|(key, histogram)| Series {
            operation_id: key.operation_id.clone(),
            status_code: key.status_code,
            labels: key.labels.clone(),
            count: histogram.count,
            total_us: histogram.total_us,
            buckets: BUCKET_BOUNDS_US
                .iter()
                .copied()
                .map(Some)
                .chain(std::iter::once(None))
                .zip(&histogram.buckets)
                .map(|(le_us, count)| Bucket { le_us, count: *count })
                .collect(),
        })
        .collect();
    Ok(HttpResponseOk(series))
}
