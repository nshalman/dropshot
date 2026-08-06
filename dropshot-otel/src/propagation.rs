// Copyright 2026 Oxide Computer Company
//! W3C trace context propagation for dropshot request spans.
//!
//! Dropshot (with its `tracing` feature) records the raw `traceparent` and
//! `tracestate` request headers as fields on each request span, but takes no
//! position on what they mean.  [`TraceContextLayer`] gives them meaning: it
//! wraps a [`tracing_opentelemetry::OpenTelemetryLayer`] and, whenever a new
//! span carries a `traceparent` field, extracts the remote OpenTelemetry
//! context from those fields and attaches it while the inner layer builds the
//! span.  The inner layer picks it up as the span's parent, linking the
//! request span into the caller's distributed trace.
//!
//! Extraction uses the global text map propagator, which
//! [`crate::Builder::install`] sets to the W3C `TraceContextPropagator`.

use opentelemetry::propagation::Extractor;
use std::any::TypeId;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// A [`Layer`] wrapping an [`OpenTelemetryLayer`] to smooth over two gaps in
/// its handling of dropshot's request spans:
///
/// * new spans carrying `traceparent`/`tracestate` fields are parented into
///   the distributed trace those fields describe;
/// * `otel.name` values recorded after a span has started still rename the
///   OpenTelemetry span (the inner layer applies them only before the span
///   starts, but dropshot can only name a request span for its endpoint
///   after routing).
///
/// All other behavior is delegated unchanged.
pub struct TraceContextLayer<S, T> {
    inner: OpenTelemetryLayer<S, T>,
}

impl<S, T> TraceContextLayer<S, T> {
    pub fn new(inner: OpenTelemetryLayer<S, T>) -> Self {
        Self { inner }
    }
}

/// Captures the `traceparent`/`tracestate` span fields (as a
/// [`tracing::field::Visit`]) and presents them to the propagator (as an
/// [`Extractor`]).
#[derive(Default)]
struct TraceHeaders {
    traceparent: Option<String>,
    tracestate: Option<String>,
}

impl tracing::field::Visit for TraceHeaders {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "traceparent" => self.traceparent = Some(value.to_string()),
            "tracestate" => self.tracestate = Some(value.to_string()),
            _ => (),
        }
    }

    fn record_debug(
        &mut self,
        _field: &tracing::field::Field,
        _value: &dyn std::fmt::Debug,
    ) {
    }
}

/// Captures an `otel.name` field recorded after span creation.
#[derive(Default)]
struct SpanRename {
    name: Option<String>,
}

impl tracing::field::Visit for SpanRename {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "otel.name" {
            self.name = Some(value.to_string());
        }
    }

    fn record_debug(
        &mut self,
        _field: &tracing::field::Field,
        _value: &dyn std::fmt::Debug,
    ) {
    }
}

impl Extractor for TraceHeaders {
    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "traceparent" => self.traceparent.as_deref(),
            "tracestate" => self.tracestate.as_deref(),
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&str> {
        [
            self.traceparent.as_ref().map(|_| "traceparent"),
            self.tracestate.as_ref().map(|_| "tracestate"),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

impl<S, T> Layer<S> for TraceContextLayer<S, T>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    T: opentelemetry::trace::Tracer + 'static,
    T::Span: Send + Sync,
{
    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, S>,
    ) {
        let mut headers = TraceHeaders::default();
        attrs.record(&mut headers);
        if headers.traceparent.is_some() {
            let parent_cx =
                opentelemetry::global::get_text_map_propagator(|propagator| {
                    propagator.extract(&headers)
                });
            // The inner layer parents contextual root spans from the
            // currently-attached OpenTelemetry context; attach the extracted
            // remote context for exactly the duration of its on_new_span.
            let _guard = parent_cx.attach();
            self.inner.on_new_span(attrs, id, ctx);
        } else {
            self.inner.on_new_span(attrs, id, ctx);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        self.inner.on_record(id, values, ctx);

        // The inner layer applies an `otel.name` update only while the span
        // is still being built; once the span has started, it drops the
        // rename.  Apply it to the started span ourselves.
        let mut rename = SpanRename::default();
        values.record(&mut rename);
        if let Some(name) = rename.name {
            tracing::dispatcher::get_default(|dispatch| {
                if let Some(otel_cx) =
                    tracing_opentelemetry::get_otel_context(id, dispatch)
                {
                    use opentelemetry::trace::TraceContextExt;
                    otel_cx.span().update_name(name.clone());
                }
            });
        }
    }

    fn on_follows_from(&self, id: &Id, follows: &Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(id, follows, ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        self.inner.on_event(event, ctx);
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }

    // The inner layer implements downcast_raw so that
    // `OpenTelemetrySpanExt` methods (`Span::context()`, `set_parent()`, ...)
    // can find it through the layer stack; keep that working.
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        if id == TypeId::of::<Self>() {
            return Some(self as *const Self as *const ());
        }
        unsafe { self.inner.downcast_raw(id) }
    }
}
