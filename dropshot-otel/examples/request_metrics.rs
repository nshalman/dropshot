// Copyright 2026 Oxide Computer Company
//! Example of per-request metrics for a Dropshot server, derived from
//! dropshot's request spans with no per-endpoint instrumentation code.
//!
//! This example keeps a latency histogram per (endpoint, status code,
//! labels) in memory and serves it at `/metrics`.  The histograms use
//! [OpenHistogram]'s log-linear bins: each value is counted in the bin of
//! values that share its first two significant digits (so 1.0-1.1ms,
//! 1.1-1.2ms, ..., 9.9-10ms, 10-11ms, ...).  That keeps every bin within 10%
//! (and mostly much less) of its values, at any scale, with no boundaries to
//! choose; and since every histogram has the same bins, histograms from
//! different processes combine by adding counts.
//!
//! ```bash
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
//!
//! To send request metrics to an OpenTelemetry backend instead, no recorder
//! is needed: set `OTEL_EXPORTER_OTLP_ENDPOINT`, and `install()` exports the
//! standard `http.server.request.duration` metric.  OpenTelemetry's
//! counterpart to these histograms is the exponential histogram; see
//! examples/otel.rs for choosing it.
//!
//! [OpenHistogram]: https://openhistogram.io/

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

/// A histogram with log-linear bins; see the top of this file.
#[derive(Default)]
struct Histogram {
    count: u64,
    total_ns: u64,
    /// Counts of values by bin, keyed by each bin's lower bound.  Only bins
    /// with values are present.
    bins: BTreeMap<u64, u64>,
}

impl Histogram {
    fn record(&mut self, value: u64) {
        self.count += 1;
        self.total_ns = self.total_ns.saturating_add(value);
        let (lower_bound, _) = bin(value);
        *self.bins.entry(lower_bound).or_default() += 1;
    }
}

/// Returns the lower bound and width of the bin containing `value`: the
/// bin of values with its first two significant digits.  (Values below 100
/// have bins of width 1, so are counted exactly.)
fn bin(value: u64) -> (u64, u64) {
    let mut width = 1;
    while value / width >= 100 {
        width *= 10;
    }
    (value / width * width, width)
}

impl Metrics {
    fn record(&self, request: &CompletedRequest) {
        let key = SeriesKey {
            operation_id: request.operation_id.clone(),
            status_code: request.status_code,
            labels: request.labels.clone(),
        };
        let latency_ns =
            u64::try_from(request.duration.as_nanos()).unwrap_or(u64::MAX);
        self.series.lock().unwrap().entry(key).or_default().record(latency_ns);
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
    total_ns: u64,
    /// The bins with any requests in them, in increasing order.
    bins: Vec<Bin>,
}

/// The number of requests with latencies in `[min_ns, max_ns)`.
#[derive(Serialize, JsonSchema)]
struct Bin {
    min_ns: u64,
    max_ns: u64,
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
            total_ns: histogram.total_ns,
            bins: histogram
                .bins
                .iter()
                .map(|(&min_ns, &count)| {
                    let (_, width) = bin(min_ns);
                    Bin { min_ns, max_ns: min_ns.saturating_add(width), count }
                })
                .collect(),
        })
        .collect();
    Ok(HttpResponseOk(series))
}

#[cfg(test)]
mod test {
    use super::bin;

    #[test]
    fn test_bin() {
        // Values below 100 are counted exactly.
        assert_eq!(bin(0), (0, 1));
        assert_eq!(bin(7), (7, 1));
        assert_eq!(bin(99), (99, 1));
        // Above that, bins hold values with the same first two digits.
        assert_eq!(bin(100), (100, 10));
        assert_eq!(bin(109), (100, 10));
        assert_eq!(bin(110), (110, 10));
        assert_eq!(bin(999), (990, 10));
        assert_eq!(bin(1_000), (1_000, 100));
        assert_eq!(bin(1_234_567), (1_200_000, 100_000));
        assert_eq!(bin(98_765_432_100), (98_000_000_000, 1_000_000_000));
        // A bin's lower bound is in the same bin.
        assert_eq!(bin(1_200_000), (1_200_000, 100_000));
        // The largest values don't overflow.
        assert_eq!(
            bin(u64::MAX),
            (18_000_000_000_000_000_000, 1_000_000_000_000_000_000)
        );
    }
}
