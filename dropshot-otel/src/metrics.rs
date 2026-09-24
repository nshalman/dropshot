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
