// Copyright 2026 Oxide Computer Company
//! Internal per-request instrumentation.
//!
//! The request path reports a small, fixed set of events: a request starts,
//! and then it either completes with a status code, completes with an error,
//! or is cancelled because the client disconnected.  This module gives those
//! events a single seam so that the request-handling code in `server.rs`
//! stays free of feature gates: [`RequestInstrumentation`] fires the USDT
//! probes (under the `usdt-probes` feature) and records the request span
//! (under the `tracing` feature) for each event, and with no instrumentation
//! features enabled it is a zero-sized type whose methods are inlineable
//! no-ops.
//!
//! The `tracing` feature's span field contract is part of the public
//! documentation; see "Tracing" in the crate docs.

#[cfg(feature = "usdt-probes")]
use crate::dtrace::probes;
#[cfg(feature = "tracing")]
use crate::router::HttpRouter;
use crate::router::RouterLookupResult;
use crate::server::{DropshotState, ServerContext};
use hyper::Request;
use hyper::body::Incoming;
#[cfg(feature = "tracing")]
use std::borrow::Cow;
#[cfg(not(feature = "usdt-probes"))]
use std::marker::PhantomData;
use std::net::SocketAddr;

/// Instrumentation handle for a single HTTP request.
///
/// Created (and the request-start event reported) just before the request is
/// routed and dispatched.  Callers must report the request's final
/// disposition through exactly one of the completion methods.
///
/// The request id is borrowed rather than copied so that, with probes
/// compiled in but not enabled, reporting costs no allocation until a probe
/// actually fires.
pub(crate) struct RequestInstrumentation<'a> {
    #[cfg(feature = "usdt-probes")]
    request_id: &'a str,
    #[cfg(not(feature = "usdt-probes"))]
    request_id: PhantomData<&'a str>,
    #[cfg(feature = "usdt-probes")]
    local_addr: SocketAddr,
    #[cfg(feature = "usdt-probes")]
    remote_addr: SocketAddr,
    #[cfg(feature = "tracing")]
    span: tracing::Span,
}

#[cfg_attr(
    not(any(feature = "usdt-probes", feature = "tracing")),
    allow(unused_variables)
)]
impl<'a> RequestInstrumentation<'a> {
    /// Reports the start of request handling.
    pub fn start<C: ServerContext>(
        server: &DropshotState<C>,
        request: &Request<Incoming>,
        request_id: &'a str,
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
            request_id,
            #[cfg(not(feature = "usdt-probes"))]
            request_id: PhantomData,
            #[cfg(feature = "usdt-probes")]
            local_addr: server.local_addr,
            #[cfg(feature = "usdt-probes")]
            remote_addr,
            #[cfg(feature = "tracing")]
            span: request_span(server, request, request_id, remote_addr),
        }
    }

    /// Reports that a response with the given status code was produced.
    pub fn responded(&self, status_code: u16) {
        #[cfg(feature = "usdt-probes")]
        probes::request__done!(|| {
            crate::dtrace::ResponseInfo {
                id: self.request_id.to_string(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                status_code,
                message: "".to_string(),
            }
        });

        #[cfg(feature = "tracing")]
        self.record_status_code(status_code);
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
                id: self.request_id.to_string(),
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
            self.record_status_code(status_code);
            if is_server_error(status_code) {
                self.span.record("otel.status_description", message_internal);
            }
            self.span.record("dropshot.error.message", message_internal);
            if let Some(external) = message_external {
                self.span.record("dropshot.error.message_external", external);
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
                id: self.request_id.to_string(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                status_code: 499,
                message: String::from(
                    "client disconnected before response returned",
                ),
            }
        });

        // No response was sent, so unlike the probe, the span records no
        // status code.
        #[cfg(feature = "tracing")]
        {
            self.span.record("error.type", "client_disconnect");
            self.span.record("otel.status_code", "ERROR");
            self.span.record(
                "otel.status_description",
                "client disconnected before response returned",
            );
        }
    }

    /// Records a response status code on the span, marking 5xx responses as
    /// errors.  Other status codes are not errors for an HTTP server span.
    #[cfg(feature = "tracing")]
    fn record_status_code(&self, status_code: u16) {
        self.span.record("http.response.status_code", i64::from(status_code));
        if is_server_error(status_code) {
            self.span.record("error.type", status_code.to_string().as_str());
            self.span.record("otel.status_code", "ERROR");
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

/// Records the resolved endpoint (its route template and operation id) on
/// the current request span.  Called from within the request span's scope
/// once routing has succeeded.
#[cfg_attr(not(feature = "tracing"), allow(unused_variables))]
pub(crate) fn record_route<C: ServerContext>(
    method: &http::Method,
    lookup_result: &RouterLookupResult<C>,
) {
    #[cfg(feature = "tracing")]
    {
        let span = tracing::Span::current();
        // Skip building the new name when nothing is listening.
        if span.is_disabled() {
            return;
        }
        let route = lookup_result.route.as_str();
        span.record("http.route", route);
        span.record(
            "dropshot.operation_id",
            lookup_result.endpoint.operation_id.as_str(),
        );
        // The router matched the method, so it is known: report it in the
        // canonical form the router matched it in.
        span.record(
            "otel.name",
            format!("{} {}", method.as_str().to_ascii_uppercase(), route)
                .as_str(),
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

/// HTTP methods that the OpenTelemetry semantic conventions define as
/// "known": those of RFC 9110, PATCH (RFC 5789), and QUERY.
#[cfg(feature = "tracing")]
const STANDARD_METHODS: &[&str] = &[
    "CONNECT", "DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT",
    "QUERY", "TRACE",
];

/// Returns the request method as the span reports it, and the method as sent
/// if that differs.  A method is known if it is a standard one or some
/// endpoint handles it.  Dropshot routes methods case-insensitively, so a
/// method whose uppercase form is known is reported in that canonical form;
/// any other method is reported as `_OTHER`.
#[cfg(feature = "tracing")]
fn span_method<'m, C: ServerContext>(
    method: &'m http::Method,
    router: &HttpRouter<C>,
) -> (Cow<'m, str>, Option<&'m str>) {
    let is_known = |name: &str| {
        STANDARD_METHODS.contains(&name) || router.handles_method(name)
    };
    let sent = method.as_str();
    if is_known(sent) {
        return (Cow::Borrowed(sent), None);
    }
    let uppercase = sent.to_ascii_uppercase();
    if is_known(&uppercase) {
        (Cow::Owned(uppercase), Some(sent))
    } else {
        (Cow::Borrowed("_OTHER"), Some(sent))
    }
}

#[cfg(feature = "tracing")]
fn is_server_error(status_code: u16) -> bool {
    (500..600).contains(&status_code)
}

/// Creates the per-request span.  See "Tracing" in the crate docs for the
/// field contract.
#[cfg(feature = "tracing")]
fn request_span<C: ServerContext>(
    server: &DropshotState<C>,
    request: &Request<Incoming>,
    request_id: &str,
    remote_addr: SocketAddr,
) -> tracing::Span {
    use http::Version;
    use tracing::field::Empty;

    let using_tls = server.using_tls();
    let (method, method_original) =
        span_method(request.method(), &server.router);
    // Span names use `HTTP` in place of `_OTHER`.
    let name_method = if method == "_OTHER" { "HTTP" } else { &method };
    let header_str =
        |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let scheme = if using_tls { "https" } else { "http" };
    let protocol_version = match request.version() {
        Version::HTTP_09 => Some("0.9"),
        Version::HTTP_10 => Some("1.0"),
        Version::HTTP_11 => Some("1.1"),
        Version::HTTP_2 => Some("2"),
        Version::HTTP_3 => Some("3"),
        _ => None,
    };
    let span = tracing::info_span!(
        "dropshot_request",
        http.request.method = method.as_ref(),
        http.request.method_original = method_original,
        url.path = request.uri().path(),
        url.query = request.uri().query(),
        url.scheme = scheme,
        network.protocol.version = protocol_version,
        server.address = Empty,
        server.port = Empty,
        client.address = %remote_addr.ip(),
        network.peer.address = %remote_addr.ip(),
        // Numeric fields are recorded as i64: unsigned values fall through
        // some tracing subscribers' visitors as stringified debug output
        // (tracing-opentelemetry's span visitor, for one, has no u64 case).
        network.peer.port = i64::from(remote_addr.port()),
        user_agent.original = header_str("user-agent"),
        http.request.header.traceparent = header_str("traceparent"),
        http.request.header.tracestate = header_str("tracestate"),
        dropshot.request_id = request_id,
        otel.kind = "server",
        otel.name = name_method,
        http.route = Empty,
        dropshot.operation_id = Empty,
        http.response.status_code = Empty,
        error.type = Empty,
        otel.status_code = Empty,
        otel.status_description = Empty,
        dropshot.error.message = Empty,
        dropshot.error.message_external = Empty,
    );

    // The server's name and port as the client addressed it: from the
    // request URI's authority (as for HTTP/2's `:authority`), else from the
    // `Host` header.  Only worth parsing if the span is recorded.
    if !span.is_disabled() {
        let authority = request.uri().authority().cloned().or_else(|| {
            header_str("host")?.parse::<http::uri::Authority>().ok()
        });
        if let Some(authority) = authority {
            // IPv6 literals are bracketed in authorities, but not in
            // `server.address`.
            let host =
                authority.host().trim_start_matches('[').trim_end_matches(']');
            let port = authority.port_u16().unwrap_or(if using_tls {
                443
            } else {
                80
            });
            span.record("server.address", host);
            span.record("server.port", i64::from(port));
        }
    }
    span
}
