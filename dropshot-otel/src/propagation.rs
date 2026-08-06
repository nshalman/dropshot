// Copyright 2026 Oxide Computer Company
//! W3C trace context propagation for dropshot request spans.
//!
//! Dropshot (with its `tracing` feature) records the raw `traceparent` and
//! `tracestate` request headers on each request span, as the
//! `http.request.header.traceparent` and `http.request.header.tracestate`
//! fields, but takes no position on what they mean.  [`TraceContextLayer`]
//! gives them meaning: it wraps a
//! [`tracing_opentelemetry::OpenTelemetryLayer`] and, whenever a new span
//! carries a `traceparent` header field, extracts the remote OpenTelemetry
//! context from those fields and attaches it while the inner layer builds the
//! span.  The inner layer picks it up as the span's parent, linking the
//! request span into the caller's distributed trace.
//!
//! Extraction uses the layer's own W3C [`TraceContextPropagator`], so it
//! does not depend on the global OpenTelemetry propagator.

use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use std::any::TypeId;
use std::sync::OnceLock;
use tracing::dispatcher::WeakDispatch;
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Subscriber};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// A [`Layer`] wrapping an [`OpenTelemetryLayer`] to smooth over two gaps in
/// its handling of dropshot's request spans:
///
/// * new spans carrying `http.request.header.traceparent` (and
///   `.tracestate`) fields are parented into the distributed trace those
///   fields describe;
/// * `otel.name` values recorded after a span has started still rename the
///   OpenTelemetry span (the inner layer applies them only before the span
///   starts, but dropshot can only name a request span for its endpoint
///   after routing).
///
/// All other behavior is delegated unchanged.
pub struct TraceContextLayer<S, T> {
    inner: OpenTelemetryLayer<S, T>,
    propagator: TraceContextPropagator,
    /// The dispatcher this layer is part of, for finding the OpenTelemetry
    /// span behind a tracing span id.
    dispatch: OnceLock<WeakDispatch>,
}

impl<S, T> TraceContextLayer<S, T> {
    /// Wraps `inner`, which does the actual OpenTelemetry work.
    pub fn new(inner: OpenTelemetryLayer<S, T>) -> Self {
        Self {
            inner,
            propagator: TraceContextPropagator::new(),
            dispatch: OnceLock::new(),
        }
    }
}

/// The span fields dropshot records the trace context request headers in.
const TRACEPARENT_FIELD: &str = "http.request.header.traceparent";
const TRACESTATE_FIELD: &str = "http.request.header.tracestate";

/// Captures the trace context header span fields (as a
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
            TRACEPARENT_FIELD if is_well_formed_traceparent(value) => {
                self.traceparent = Some(value.to_string())
            }
            TRACESTATE_FIELD => self.tracestate = Some(value.to_string()),
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

/// Checks the shape of a W3C `traceparent` header's first four fields
/// (version, trace id, parent id, and flags): exactly 2, 32, 16, and 2
/// lowercase hex digits.  The propagator checks the version, the field count,
/// and case, but parses the fields as numbers without checking their lengths
/// or excluding a leading `+`, so it would otherwise accept e.g. a short
/// trace id.
fn is_well_formed_traceparent(value: &str) -> bool {
    let fields: Vec<&str> = value.split('-').collect();
    if fields.len() < 4 {
        return false;
    }
    fields.iter().zip([2, 32, 16, 2]).all(|(field, len)| {
        field.len() == len
            && field.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Span extension marking a span that has been entered, and so (for the
/// inner layer) started.
struct Started;

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

    // Values recorded with `%` or `?` arrive here; format them the way the
    // inner layer does for `otel.name`.
    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        if field.name() == "otel.name" {
            self.name = Some(format!("{:?}", value));
        }
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
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        let _ = self.dispatch.set(subscriber.downgrade());
        self.inner.on_register_dispatch(subscriber);
    }

    fn on_new_span(
        &self,
        attrs: &Attributes<'_>,
        id: &Id,
        ctx: Context<'_, S>,
    ) {
        let mut headers = TraceHeaders::default();
        attrs.record(&mut headers);
        if headers.traceparent.is_some() {
            let parent_cx = self.propagator.extract(&headers);
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
        let started = ctx
            .span(id)
            .is_some_and(|span| span.extensions().get::<Started>().is_some());
        self.inner.on_record(id, values, ctx);

        // The inner layer applies an `otel.name` update only while the span
        // is still being built; once the span has started, it drops the
        // rename.  Apply it to the started span ourselves.  (Not before: that
        // would start the span early.)
        if !started {
            return;
        }
        let mut rename = SpanRename::default();
        values.record(&mut rename);
        let Some(name) = rename.name else {
            return;
        };
        let apply = |dispatch: &Dispatch| {
            // Only via a dispatcher this layer is part of: span ids are
            // per-subscriber, so another subscriber's span may share this
            // span's id.
            let ours = dispatch
                .downcast_ref::<Self>()
                .is_some_and(|layer| std::ptr::eq(layer, self));
            if !ours {
                return;
            }
            if let Some(otel_cx) =
                tracing_opentelemetry::get_otel_context(id, dispatch)
            {
                use opentelemetry::trace::TraceContextExt;
                otel_cx.span().update_name(name.clone());
            }
        };
        // The span belongs to this layer's dispatcher, which need not be the
        // thread's current default.  But some layer wrappers (notably
        // `Option<Layer>`) don't pass `on_register_dispatch` through, leaving
        // this layer without its dispatcher; then try the current default.
        match self.dispatch.get().and_then(WeakDispatch::upgrade) {
            Some(dispatch) => apply(&dispatch),
            None => tracing::dispatcher::get_default(apply),
        }
    }

    fn on_follows_from(&self, id: &Id, follows: &Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(id, follows, ctx);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        self.inner.on_event(event, ctx);
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        // The inner layer starts the OpenTelemetry span on its first entry.
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().replace(Started);
        }
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

#[cfg(test)]
mod test {
    use super::TraceContextLayer;
    use opentelemetry::trace::{SpanId, TraceId, TracerProvider as _};
    use opentelemetry_sdk::trace::{
        InMemorySpanExporter, SdkTracerProvider, SpanData,
    };
    use tracing::field::Empty;
    use tracing_subscriber::layer::SubscriberExt;

    const TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
    const SPAN_ID: &str = "b7ad6b7169203331";

    /// Returns a subscriber exporting through a `TraceContextLayer` into the
    /// returned in-memory exporter, and the tracer provider, which must be
    /// kept alive for spans to be exported.  Nothing here touches the global
    /// OpenTelemetry propagator.
    fn test_subscriber() -> (
        impl tracing::Subscriber + Send + Sync,
        InMemorySpanExporter,
        SdkTracerProvider,
    ) {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber =
            tracing_subscriber::registry().with(TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("test")),
            ));
        (subscriber, exporter, provider)
    }

    /// Creates, enters, and closes one span with the given `traceparent`
    /// field under a fresh subscriber, and returns the exported span.
    fn span_with_traceparent(traceparent: &str) -> SpanData {
        let (subscriber, exporter, _provider) = test_subscriber();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "request",
                http.request.header.traceparent = traceparent
            );
            let _entered = span.enter();
        });
        let mut spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        spans.remove(0)
    }

    #[test]
    fn test_valid_traceparent_parents_span() {
        let span =
            span_with_traceparent(&format!("00-{}-{}-01", TRACE_ID, SPAN_ID));
        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_hex(TRACE_ID).unwrap()
        );
        assert_eq!(span.parent_span_id, SpanId::from_hex(SPAN_ID).unwrap());
    }

    #[test]
    fn test_malformed_traceparent_is_ignored() {
        for traceparent in [
            // Trace id two hex digits short.
            format!("00-{}-{}-01", &TRACE_ID[2..], SPAN_ID),
            // Span id two hex digits short.
            format!("00-{}-{}-01", TRACE_ID, &SPAN_ID[2..]),
            "00-abc-def-01".to_string(),
            // Right length, but with a sign the number parser accepts.
            format!("00-+{}-{}-01", &TRACE_ID[1..], SPAN_ID),
            // Version 00 has exactly four fields.
            format!("00-{}-{}-01-extra", TRACE_ID, SPAN_ID),
            format!("00-{}-{}-01", TRACE_ID.to_uppercase(), SPAN_ID),
            format!("ff-{}-{}-01", TRACE_ID, SPAN_ID),
            "garbage".to_string(),
        ] {
            let span = span_with_traceparent(&traceparent);
            assert_eq!(
                span.parent_span_id,
                SpanId::INVALID,
                "traceparent {:?} parented the span",
                traceparent
            );
        }
    }

    #[test]
    fn test_rename_after_start() {
        let (subscriber, exporter, _provider) = test_subscriber();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            let _entered = span.enter();
            span.record("otel.name", "renamed as str");
        });
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans[0].name, "renamed as str");
    }

    #[test]
    fn test_rename_after_start_with_display_value() {
        let (subscriber, exporter, _provider) = test_subscriber();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            let _entered = span.enter();
            span.record("otel.name", tracing::field::display("renamed"));
        });
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans[0].name, "renamed");
    }

    /// `Option<Layer>` does not forward `on_register_dispatch`, so a wrapped
    /// layer never learns its dispatcher; renaming must still work.
    #[test]
    fn test_rename_when_wrapped_in_option() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber =
            tracing_subscriber::registry().with(Some(TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("test")),
            )));
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            let _entered = span.enter();
            span.record("otel.name", "renamed");
        });
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans[0].name, "renamed");
    }

    /// Without its dispatcher (see above), the layer must still never rename
    /// a span belonging to some other subscriber that happens to be the
    /// thread's default.  (Span ids are per-subscriber, so the other
    /// subscriber may well have a span with the same id.)
    #[test]
    fn test_rename_never_reaches_another_subscriber() {
        let wrapped = |provider: &SdkTracerProvider| {
            tracing_subscriber::registry().with(Some(TraceContextLayer::new(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer("test")),
            )))
        };
        let (_, _, provider) = test_subscriber();
        let (_, other_exporter, other_provider) = test_subscriber();
        let (subscriber, other_subscriber) =
            (wrapped(&provider), wrapped(&other_provider));
        let span = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            drop(span.enter());
            span
        });
        tracing::subscriber::with_default(other_subscriber, || {
            let _other = tracing::info_span!("other").entered();
            span.record("otel.name", "renamed");
        });
        drop(span);
        let other_spans = other_exporter.get_finished_spans().unwrap();
        assert!(
            other_spans.iter().all(|s| s.name == "other"),
            "{:?}",
            other_spans.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    /// Renaming a span before it starts is the inner layer's business; doing
    /// it here would start the span early, losing a parent set afterwards.
    #[test]
    fn test_rename_before_start_keeps_later_parent() {
        use opentelemetry::trace::{
            SpanContext, TraceContextExt as _, TraceFlags, TraceState,
        };
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;

        let (subscriber, exporter, _provider) = test_subscriber();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            span.record("otel.name", "renamed");
            let remote = opentelemetry::Context::new()
                .with_remote_span_context(SpanContext::new(
                    TraceId::from_hex(TRACE_ID).unwrap(),
                    SpanId::from_hex(SPAN_ID).unwrap(),
                    TraceFlags::SAMPLED,
                    true,
                    TraceState::default(),
                ));
            span.set_parent(remote).unwrap();
            let _entered = span.enter();
        });
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans[0].name, "renamed");
        assert_eq!(spans[0].parent_span_id, SpanId::from_hex(SPAN_ID).unwrap());
    }

    /// The rename must reach the span's own subscriber even when a different
    /// subscriber is the thread's default at the time of recording.
    #[test]
    fn test_rename_under_another_default_subscriber() {
        let (subscriber, exporter, _provider) = test_subscriber();
        let (other_subscriber, other_exporter, _other_provider) =
            test_subscriber();
        let span = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", otel.name = Empty);
            // Start the OpenTelemetry span.
            drop(span.enter());
            span
        });
        tracing::subscriber::with_default(other_subscriber, || {
            let _other = tracing::info_span!("other").entered();
            span.record("otel.name", "renamed");
        });
        drop(span);
        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "renamed");
        let other_spans = other_exporter.get_finished_spans().unwrap();
        assert!(other_spans.iter().all(|s| s.name == "other"));
    }
}
