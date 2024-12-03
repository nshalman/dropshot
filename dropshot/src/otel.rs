// Copyright 2024 Oxide Computer Company
//! Opentelemetry tracing support
//!
//! Fields that we want to produce to provide comparable
//! functionality to reqwest-tracing[1]:
//!
//! - http.request.method
//! - url.scheme
//! - server.address
//! - server.port
//! - otel.kind
//! - otel.name
//! - otel.status_code
//! - user_agent.original
//! - http.response.status_code
//! - error.message
//! - error.cause_chain
//!
//! [1] <https://docs.rs/reqwest-tracing/0.5.4/reqwest_tracing/macro.reqwest_otel_span.html>

use opentelemetry::{
    global, trace::Span, trace::Tracer, trace::TracerProvider,
};
use opentelemetry_http::HeaderExtractor;
use opentelemetry_semantic_conventions::trace;

// - http.request.method
// - url.scheme
// - server.address
// - server.port
// - otel.kind
// - otel.name
// - otel.status_code
// - user_agent.original
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RequestInfo {
    pub id: String,
    pub local_addr: std::net::SocketAddr,
    pub remote_addr: std::net::SocketAddr,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ResponseInfo {
    pub id: String,
    pub local_addr: std::net::SocketAddr,
    pub remote_addr: std::net::SocketAddr,
    pub status_code: u16,
    pub message: String,
}

fn extract_context_from_request(
    request: &hyper::Request<hyper::body::Incoming>,
) -> opentelemetry::Context {
    global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    })
}

pub fn create_request_span(
    request: &hyper::Request<hyper::body::Incoming>,
) -> opentelemetry::global::BoxedSpan {
    let tracer_provider = global::tracer_provider();
    let scope =
        opentelemetry::InstrumentationScope::builder("dropshot_tracing")
            .with_version(env!("CARGO_PKG_VERSION"))
            .with_schema_url("https://opentelemetry.io/schemas/1.17.0")
            .build();
    let tracer = tracer_provider.tracer_with_scope(scope);
    let parent_cx = extract_context_from_request(&request);
    tracer
        .span_builder("dropshot_request") //XXX Fixme
        .with_kind(opentelemetry::trace::SpanKind::Server)
        .start_with_context(&tracer, &parent_cx)
}

pub trait TraceDropshot {
    fn trace_request(&mut self, request: RequestInfo);
    fn trace_response(&mut self, response: ResponseInfo);
}

impl TraceDropshot for opentelemetry::global::BoxedSpan {
    fn trace_request(&mut self, request: RequestInfo) {
        self.set_attributes(vec![
            opentelemetry::KeyValue::new("http.id".to_string(), request.id),
            opentelemetry::KeyValue::new(
                "http.method".to_string(),
                request.method,
            ),
            opentelemetry::KeyValue::new("http.path".to_string(), request.path),
        ]);
    }
    fn trace_response(&mut self, response: ResponseInfo) {
        self.set_attributes(vec![
            opentelemetry::KeyValue::new(
                trace::HTTP_RESPONSE_STATUS_CODE,
                i64::from(response.status_code),
            ),
            opentelemetry::KeyValue::new(
                "http.message".to_string(),
                response.message,
            ),
        ]);
    }
}
