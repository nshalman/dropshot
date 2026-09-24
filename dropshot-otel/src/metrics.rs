// Copyright 2026 Oxide Computer Company
//! Per-request metrics derived from dropshot's request spans.
//!
//! [`layer`] returns a `tracing` layer that watches for dropshot's
//! per-request spans and, as each one closes, reports a [`CompletedRequest`]:
//! how long the request took and how it was handled (endpoint, status code,
//! error type), plus any application-specific [`label`]s.  Because it works from the request span,
//! it covers every request — including those that never reach a handler (a
//! 404, a 405, a failed version check) and those whose client disconnected —
//! with no per-endpoint code.
//!
//! The layer only gathers the numbers; what to do with them (histograms,
//! counters, an export pipeline) is up to the function it is given.
//! [`crate::Builder::install`] uses it to export the standard
//! `http.server.request.duration` metric via OTLP, and
//! [`crate::Builder::with_request_metrics`] passes each request to the
//! application as well.

use opentelemetry::KeyValue;
use opentelemetry::Value;
use opentelemetry::metrics::Histogram;
use opentelemetry::metrics::Meter;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;
use std::time::Instant;
use tracing::Metadata;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

/// What dropshot recorded about one request, reported as its span closes.
///
/// Fields other than `duration` and `labels` come from the request span's
/// fields of the same meaning; see "Tracing" in dropshot's crate docs.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct CompletedRequest {
    /// `http.request.method`: the method, or `_OTHER` if unknown.
    pub method: String,
    /// `url.path`: the request path as sent.  This identifies individual
    /// resources, so it is a poor metric dimension; prefer `route`.
    pub url_path: String,
    /// `url.scheme`: `http` or `https`.
    pub url_scheme: String,
    /// `network.protocol.version`, e.g. `1.1` or `2`.
    pub protocol_version: Option<String>,
    /// `server.address`: the server's name as the client addressed it.
    pub server_address: Option<String>,
    /// `server.port`: the server's port as the client addressed it.
    pub server_port: Option<u16>,
    /// `http.route`: the matched endpoint's path template, if routing
    /// succeeded.
    pub route: Option<String>,
    /// `dropshot.operation_id`: the matched endpoint's operation id, if
    /// routing succeeded.
    pub operation_id: Option<String>,
    /// `http.response.status_code`, if a response was produced.
    pub status_code: Option<u16>,
    /// `error.type`: the status code for a 5xx response, or
    /// `client_disconnect` if the client went away first.
    pub error_type: Option<String>,
    /// Time from the request span's creation (as dropshot began handling the
    /// request) to its close (once the response was produced or the client
    /// disconnected).  Excludes sending the response body.
    pub duration: Duration,
    /// Labels attached to the request with [`label`].
    pub labels: BTreeMap<String, String>,
}

/// Returns a `tracing` layer that reports a [`CompletedRequest`] to
/// `recorder` as each dropshot request span closes.
///
/// The recorder runs synchronously on the task that handled the request, so
/// it should be quick (e.g. update an in-memory histogram).
///
/// The layer carries its own [per-layer filter], so it sees only dropshot's
/// request spans and filters nothing for the subscriber's other layers.  It
/// sees a request only if the subscriber creates that request's span, so a
/// filter applied to the whole subscriber (e.g. an `EnvFilter` added with
/// `.with(filter)`) that excludes the `dropshot::instrument` target also
/// stops metrics.  Apply such filters to the other layers individually
/// instead, with [`Layer::with_filter`]; [`crate::Builder::install`] does
/// this.
///
/// [per-layer filter]: tracing_subscriber::layer#per-layer-filtering
pub fn layer<S>(
    recorder: impl Fn(&CompletedRequest) + Send + Sync + 'static,
) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    RequestMetricsLayer { recorder: Box::new(recorder) }.with_filter(
        filter_fn(|metadata| is_request_span(metadata))
            .with_max_level_hint(LevelFilter::INFO),
    )
}

/// See [`layer`].
struct RequestMetricsLayer {
    recorder: Box<dyn Fn(&CompletedRequest) + Send + Sync>,
}

/// Per-request state, kept in the request span's extensions.
struct RequestState {
    start: Instant,
    request: CompletedRequest,
}

impl<S> Layer<S> for RequestMetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: Context<'_, S>,
    ) {
        if !is_request_span(attrs.metadata()) {
            return;
        }
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut state = RequestState {
            start: Instant::now(),
            request: CompletedRequest::default(),
        };
        attrs.record(&mut state.request);
        span.extensions_mut().insert(state);
    }

    fn on_record(
        &self,
        id: &span::Id,
        values: &span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        if let Some(state) = span.extensions_mut().get_mut::<RequestState>() {
            values.record(&mut state.request);
        }
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let state = span.extensions_mut().remove::<RequestState>();
        if let Some(RequestState { start, mut request }) = state {
            request.duration = start.elapsed();
            (self.recorder)(&request);
        }
    }
}

/// Attaches an application-specific label (e.g. the authenticated user's
/// organization) to the request being handled, to be reported in its
/// [`CompletedRequest::labels`].  A later label with the same key replaces
/// an earlier one.
///
/// Call it from anywhere within a request handler, including from within
/// the handler's own spans.  It does nothing if called outside of a request,
/// or if no metrics [`layer`] is installed.
///
/// Every distinct label value becomes a separate series for most metrics
/// backends, so use labels with a small set of possible values.
pub fn label(key: impl Into<String>, value: impl Into<String>) {
    tracing::Span::current().with_subscriber(|(id, dispatch)| {
        let Some(registry) = dispatch.downcast_ref::<Registry>() else {
            return;
        };
        let Some(span) = registry.span(id) else {
            return;
        };
        // The nearest enclosing request span, the current one included.
        let Some(request_span) =
            span.scope().find(|span| is_request_span(span.metadata()))
        else {
            return;
        };
        if let Some(state) =
            request_span.extensions_mut().get_mut::<RequestState>()
        {
            state.request.labels.insert(key.into(), value.into());
        }
    });
}

/// Records completed requests in the OpenTelemetry semantic conventions'
/// `http.server.request.duration` histogram, which [`crate::Builder::install`]
/// exports when metrics export is configured.
///
/// Each request's attributes are the conventions' standard ones, except the
/// opt-in `server.address` and `server.port` (which come from request headers,
/// so a client could use them to create unbounded numbers of series), plus
/// the request's [`label`]s.  A label whose key is one of the standard
/// attributes is ignored.
pub(crate) struct DurationHistogram(Histogram<f64>);

impl DurationHistogram {
    pub(crate) fn new(meter: &Meter) -> Self {
        DurationHistogram(
            meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description("Duration of HTTP server requests.")
                // The semantic conventions' advised bucket boundaries.
                .with_boundaries(vec![
                    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0,
                    2.5, 5.0, 7.5, 10.0,
                ])
                .build(),
        )
    }

    pub(crate) fn record(&self, request: &CompletedRequest) {
        let mut attributes = vec![
            KeyValue::new("http.request.method", request.method.clone()),
            KeyValue::new("url.scheme", request.url_scheme.clone()),
        ];
        let optional = [
            ("error.type", request.error_type.clone().map(Value::from)),
            (
                "http.response.status_code",
                request.status_code.map(|code| Value::from(i64::from(code))),
            ),
            ("http.route", request.route.clone().map(Value::from)),
            (
                "network.protocol.version",
                request.protocol_version.clone().map(Value::from),
            ),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                attributes.push(KeyValue::new(key, value));
            }
        }
        for (key, value) in &request.labels {
            if !attributes.iter().any(|kv| kv.key.as_str() == key) {
                attributes.push(KeyValue::new(key.clone(), value.clone()));
            }
        }
        self.0.record(request.duration.as_secs_f64(), &attributes);
    }
}

/// Returns whether `metadata` describes a dropshot request span.
fn is_request_span(metadata: &Metadata<'_>) -> bool {
    metadata.target() == "dropshot::instrument"
        && metadata.name() == "dropshot_request"
}

impl Visit for CompletedRequest {
    fn record_str(&mut self, field: &Field, value: &str) {
        let value = value.to_string();
        match field.name() {
            "http.request.method" => self.method = value,
            "url.path" => self.url_path = value,
            "url.scheme" => self.url_scheme = value,
            "network.protocol.version" => self.protocol_version = Some(value),
            "server.address" => self.server_address = Some(value),
            "http.route" => self.route = Some(value),
            "dropshot.operation_id" => self.operation_id = Some(value),
            "error.type" => self.error_type = Some(value),
            _ => (),
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            "server.port" => self.server_port = u16::try_from(value).ok(),
            "http.response.status_code" => {
                self.status_code = u16::try_from(value).ok()
            }
            _ => (),
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn fmt::Debug) {
        // None of the fields we report are recorded as `Debug` values.
    }
}

#[cfg(test)]
mod test {
    use super::CompletedRequest;
    use super::DurationHistogram;
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::metrics::data::AggregatedMetrics;
    use opentelemetry_sdk::metrics::data::MetricData;
    use std::time::Duration;

    #[test]
    fn test_duration_histogram() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        let histogram = DurationHistogram::new(&provider.meter("test"));

        let mut ok = CompletedRequest {
            method: "GET".to_string(),
            url_path: "/items/1".to_string(),
            url_scheme: "http".to_string(),
            protocol_version: Some("1.1".to_string()),
            server_address: Some("localhost".to_string()),
            server_port: Some(8080),
            route: Some("/items/{id}".to_string()),
            operation_id: Some("get_item".to_string()),
            status_code: Some(200),
            error_type: None,
            duration: Duration::from_millis(30),
            labels: Default::default(),
        };
        ok.labels.insert("tenant".to_string(), "acme".to_string());
        // Labels can't override the standard attributes.
        ok.labels.insert("http.route".to_string(), "/elsewhere".to_string());
        histogram.record(&ok);
        let disconnected = CompletedRequest {
            method: "_OTHER".to_string(),
            url_path: "/hang".to_string(),
            url_scheme: "https".to_string(),
            error_type: Some("client_disconnect".to_string()),
            duration: Duration::from_secs(2),
            ..Default::default()
        };
        histogram.record(&disconnected);
        provider.force_flush().unwrap();

        let metrics = exporter.get_finished_metrics().unwrap();
        let metric = metrics
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .find(|m| m.name() == "http.server.request.duration")
            .expect("no http.server.request.duration metric");
        assert_eq!(metric.unit(), "s");
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) =
            metric.data()
        else {
            panic!("not an f64 histogram: {:?}", metric.data());
        };
        let points: Vec<_> = histogram
            .data_points()
            .map(|point| {
                let mut attributes: Vec<KeyValue> =
                    point.attributes().cloned().collect();
                attributes.sort_by(|a, b| a.key.cmp(&b.key));
                (attributes, point)
            })
            .collect();
        assert_eq!(points.len(), 2, "{:#?}", histogram);
        let find = |method: &str| {
            points
                .iter()
                .find(|(attributes, _)| {
                    attributes.contains(&KeyValue::new(
                        "http.request.method",
                        method.to_string(),
                    ))
                })
                .unwrap_or_else(|| panic!("no data point for {}", method))
        };

        let (attributes, point) = find("GET");
        assert_eq!(
            *attributes,
            [
                KeyValue::new("http.request.method", "GET"),
                KeyValue::new("http.response.status_code", 200),
                KeyValue::new("http.route", "/items/{id}"),
                KeyValue::new("network.protocol.version", "1.1"),
                KeyValue::new("tenant", "acme"),
                KeyValue::new("url.scheme", "http"),
            ]
        );
        assert_eq!(point.count(), 1);
        assert!((point.sum() - 0.030).abs() < 1e-9, "{}", point.sum());
        assert_eq!(
            point.bounds().collect::<Vec<_>>(),
            [
                0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0,
                2.5, 5.0, 7.5, 10.0
            ]
        );

        let (attributes, point) = find("_OTHER");
        assert_eq!(
            *attributes,
            [
                KeyValue::new("error.type", "client_disconnect"),
                KeyValue::new("http.request.method", "_OTHER"),
                KeyValue::new("url.scheme", "https"),
            ]
        );
        assert_eq!(point.count(), 1);
        assert!((point.sum() - 2.0).abs() < 1e-9, "{}", point.sum());
    }
}
