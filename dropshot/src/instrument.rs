// Copyright 2026 Oxide Computer Company
//! Internal per-request instrumentation.
//!
//! The request path reports a small, fixed set of events: a request starts,
//! and then it either completes with a status code, completes with an error,
//! is cancelled because the client disconnected, or ends because the handler
//! panicked.  This module gives those events a single seam so that the
//! request-handling code in `server.rs` stays free of feature gates:
//! [`RequestInstrumentation`] fires the USDT probes (under the
//! `usdt-probes` feature) and records the request span (under the `tracing`
//! feature) for each event, and with no instrumentation features enabled it
//! is a zero-sized type whose methods are inlineable no-ops.
//!
//! # Tracing span field contract
//!
//! When the `tracing` feature is enabled, each request gets one
//! [`tracing::Span`] named `dropshot_request`.  Dropshot has no opinion
//! about what (if anything) consumes these spans: with no `tracing`
//! subscriber installed they are disabled at the callsite and cost almost
//! nothing, and this module is deliberately dependency-free beyond the
//! `tracing` facade.  In particular, OpenTelemetry integration (exporters,
//! propagation, and the like) is expected to live entirely in the consuming
//! application or a companion crate (see `dropshot-otel`), built on this
//! contract:
//!
//! * `http.request.method`, `http.request.uri`, `http.request.id`,
//!   `client.address`, `client.port`, `user_agent.original`: recorded at
//!   span creation from the incoming request.
//! * `traceparent`, `tracestate`: the raw W3C trace context headers from the
//!   request, recorded at span creation only if present.  A subscriber layer
//!   can use these to parent the span into a distributed trace.
//! * `otel.kind`: always `"server"`; `otel.name`: a low-cardinality span
//!   name, initially the request method.  These field names follow the
//!   `tracing-opentelemetry` conventions but are plain strings here.
//! * `dropshot.operation_id`: recorded once routing succeeds; `otel.name` is
//!   updated to `"{method} {operation_id}"` at the same time.
//! * `http.response.status_code`: recorded when a response (including an
//!   error response) is produced.
//! * `error`, `error.type`, `error.message`, `error.message.external`:
//!   recorded for error responses, client disconnects
//!   (`error.type = "client_disconnect"`, with a synthetic 499 status
//!   code), and handler panics that propagate (`error.type = "panic"`, with
//!   no status code recorded: the request never produced one).
//!   `error.message` is the internal (operator-facing) message;
//!   `error.message.external` is the client-facing message, recorded only
//!   when known (dropshot's own `HttpError`s have one; user-defined error
//!   types serialize their client-facing content directly into the response
//!   body).  Together these carry the same information reported by the USDT
//!   probes, which report the external message when known and the internal
//!   message otherwise.

#[cfg(feature = "usdt-probes")]
use crate::dtrace::probes;
use crate::server::{DropshotState, ServerContext};
use hyper::Request;
use hyper::body::Incoming;
use std::net::SocketAddr;

/// Instrumentation handle for a single HTTP request.
///
/// Created (and the request-start event reported) at the top of request
/// handling; the request's final disposition is reported through exactly one
/// of the completion methods.
pub(crate) struct RequestInstrumentation {
    #[cfg(feature = "usdt-probes")]
    request_id: String,
    #[cfg(feature = "usdt-probes")]
    local_addr: SocketAddr,
    #[cfg(feature = "usdt-probes")]
    remote_addr: SocketAddr,
    #[cfg(feature = "tracing")]
    span: tracing::Span,
}

#[cfg_attr(
    not(any(feature = "usdt-probes", feature = "tracing")),
    allow(unused_variables, clippy::unused_self)
)]
impl RequestInstrumentation {
    /// Reports the start of request handling.
    // `server` is used only for the USDT probe.
    #[cfg_attr(not(feature = "usdt-probes"), allow(unused_variables))]
    pub fn start<C: ServerContext>(
        server: &DropshotState<C>,
        request: &Request<Incoming>,
        request_id: &str,
        remote_addr: SocketAddr,
    ) -> Self {
        #[cfg(feature = "usdt-probes")]
        probes::request__start!(|| {
            let uri = request.uri();
            crate::dtrace::RequestInfo {
                id: request_id.to_string(),
                local_addr: server.local_addr,
                remote_addr,
                method: request.method().to_string(),
                path: uri.path().to_string(),
                query: uri.query().map(|x| x.to_string()),
            }
        });

        Self {
            #[cfg(feature = "usdt-probes")]
            request_id: request_id.to_string(),
            #[cfg(feature = "usdt-probes")]
            local_addr: server.local_addr,
            #[cfg(feature = "usdt-probes")]
            remote_addr,
            #[cfg(feature = "tracing")]
            span: request_span(request, request_id, remote_addr),
        }
    }

    /// Reports that a response with the given status code was produced.
    pub fn responded(&self, status_code: u16) {
        #[cfg(feature = "usdt-probes")]
        probes::request__done!(|| {
            crate::dtrace::ResponseInfo {
                id: self.request_id.clone(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                status_code,
                message: "".to_string(),
            }
        });

        #[cfg(feature = "tracing")]
        self.span.record("http.response.status_code", i64::from(status_code));
    }

    /// Reports that an error response with the given status code was
    /// produced.
    pub fn errored(
        &self,
        status_code: u16,
        message_external: Option<&str>,
        message_internal: &str,
    ) {
        #[cfg(feature = "usdt-probes")]
        probes::request__done!(|| {
            crate::dtrace::ResponseInfo {
                id: self.request_id.clone(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                status_code,
                message: message_external
                    .unwrap_or(message_internal)
                    .to_string(),
            }
        });

        #[cfg(feature = "tracing")]
        {
            self.span
                .record("http.response.status_code", i64::from(status_code));
            self.span.record("error", true);
            self.span.record("error.message", message_internal);
            if let Some(external) = message_external {
                self.span.record("error.message.external", external);
            }
        }
    }

    /// Reports that the client disconnected before a response was returned.
    /// 499 is the non-standard status code popularized by nginx to mean
    /// "client disconnected".
    pub fn disconnected(&self) {
        #[cfg(feature = "usdt-probes")]
        probes::request__done!(|| {
            crate::dtrace::ResponseInfo {
                id: self.request_id.clone(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                status_code: 499,
                message: String::from(
                    "client disconnected before response returned",
                ),
            }
        });

        #[cfg(feature = "tracing")]
        {
            self.span.record("http.response.status_code", 499i64);
            self.span.record("error", true);
            self.span.record("error.type", "client_disconnect");
            self.span.record(
                "error.message",
                "client disconnected before response returned",
            );
        }
    }

    /// Reports that a panic unwound out of request handling.  Deliberately
    /// fires no request-done probe and records no status code on the span:
    /// the request never produced a status code, much as a process
    /// terminated by a signal has no exit code.
    pub fn panicked(&self) {
        #[cfg(feature = "tracing")]
        {
            self.span.record("error", true);
            self.span.record("error.type", "panic");
            self.span.record("error.message", "request handler panicked");
        }
    }

    /// Attaches the request span to the given future.  The span is attached
    /// rather than entered: holding a span guard across an `await` would
    /// corrupt other tasks' span contexts on this thread.
    #[cfg(feature = "tracing")]
    pub fn in_span<F: std::future::Future>(
        &self,
        fut: F,
    ) -> tracing::instrument::Instrumented<F> {
        tracing::Instrument::instrument(fut, self.span.clone())
    }

    /// Attaches the request span to the given future (no-op: no
    /// instrumentation feature that uses spans is enabled).
    #[cfg(not(feature = "tracing"))]
    pub fn in_span<F: std::future::Future>(&self, fut: F) -> F {
        fut
    }
}

/// Records the resolved endpoint on the current request span.  Called from
/// within the request span's scope once routing has succeeded.
#[cfg_attr(not(feature = "tracing"), allow(unused_variables))]
pub(crate) fn record_operation_id(method: &http::Method, operation_id: &str) {
    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        span.record("dropshot.operation_id", operation_id);
        span.record(
            "otel.name",
            format!("{} {}", method, operation_id).as_str(),
        );
    }
}

/// Attaches the current request span to the given future, keeping work
/// spawned onto another task (e.g. a detached handler) attached to the
/// request span.
#[cfg(feature = "tracing")]
pub(crate) fn in_current_span<F: std::future::Future>(
    fut: F,
) -> tracing::instrument::Instrumented<F> {
    tracing::Instrument::instrument(fut, tracing::Span::current())
}

/// Attaches the current request span to the given future (no-op: no
/// instrumentation feature that uses spans is enabled).
#[cfg(not(feature = "tracing"))]
pub(crate) fn in_current_span<F: std::future::Future>(fut: F) -> F {
    fut
}

/// Creates the per-request span.  See the module docs for the field
/// contract.
#[cfg(feature = "tracing")]
fn request_span(
    request: &Request<Incoming>,
    request_id: &str,
    remote_addr: SocketAddr,
) -> tracing::Span {
    use tracing::field::Empty;

    let header_str =
        |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    tracing::info_span!(
        "dropshot_request",
        http.request.method = %request.method(),
        http.request.uri = %request.uri(),
        http.request.id = request_id,
        // Numeric fields are recorded as i64: unsigned values fall through
        // some tracing subscribers' visitors as stringified debug output.
        client.address = %remote_addr.ip(),
        client.port = i64::from(remote_addr.port()),
        user_agent.original = header_str("user-agent"),
        traceparent = header_str("traceparent"),
        tracestate = header_str("tracestate"),
        otel.kind = "server",
        otel.name = %request.method(),
        dropshot.operation_id = Empty,
        http.response.status_code = Empty,
        error = Empty,
        error.type = Empty,
        error.message = Empty,
        error.message.external = Empty,
    )
}
