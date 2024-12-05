// Copyright 2024 Oxide Computer Company
//! Opentelemetry tracing support

#[cfg(feature = "otel-tracing")]
use crate::dtrace::{RequestInfo, ResponseInfo};
#[cfg(feature = "otel-tracing")]
use opentelemetry::{global, trace::Span, trace::Tracer};
#[cfg(feature = "otel-tracing")]
use opentelemetry_http::HeaderExtractor;
#[cfg(feature = "otel-tracing")]
use opentelemetry_semantic_conventions::trace;

#[cfg(feature = "otel-tracing")]
fn extract_context_from_request(
    request: &hyper::Request<hyper::body::Incoming>,
) -> opentelemetry::Context {
    global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    })
}

#[cfg(feature = "otel-tracing")]
pub fn create_request_span(
    request: &hyper::Request<hyper::body::Incoming>,
) -> opentelemetry::global::BoxedSpan {
    let parent_cx = extract_context_from_request(&request);
    let tracer = global::tracer("");
    tracer
        .span_builder("dropshot1") //XXX Fixme
        .with_kind(opentelemetry::trace::SpanKind::Server)
        .start_with_context(&tracer, &parent_cx)
}

#[cfg(feature = "otel-tracing")]
pub trait TraceRequest {
    fn trace_request(&mut self, request: RequestInfo);
    fn trace_error(&mut self, error: &dyn std::error::Error);
}

#[cfg(feature = "otel-tracing")]
impl TraceRequest for opentelemetry::global::BoxedSpan {
    fn trace_request(&mut self, request: RequestInfo) {
        self.set_attributes(
            vec![
            opentelemetry::KeyValue::new(
                "http.id".to_string(),
                request.id,
            ),
            opentelemetry::KeyValue::new(
                "http.method".to_string(),
                request.method,
            ),
            opentelemetry::KeyValue::new(
                "http.path".to_string(),
                request.path,
            ),
            ],
        );
    }
    fn trace_error(&mut self, error: &dyn std::error::Error) {
        self.record_error(&error)
    }
}

#[cfg(feature = "otel-tracing")]
pub trait TraceResponse {
    fn trace_response(&mut self, response: ResponseInfo);
}

#[cfg(feature = "otel-tracing")]
impl TraceResponse for opentelemetry::global::BoxedSpan {
    fn trace_response(&mut self, response: ResponseInfo) {
        self.set_attributes(
            vec![
            opentelemetry::KeyValue::new(
                trace::HTTP_RESPONSE_STATUS_CODE,
                i64::from(response.status_code),
            ),
            opentelemetry::KeyValue::new(
                "http.message".to_string(),
                response.message,
            ),
            ],
        );
    }
}

/*
#[cfg_attr(any, feature = "usdt-probes", feature = "otel-tracing")]
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct RequestInfo {
    pub id: String,
    pub local_addr: std::net::SocketAddr,
    pub remote_addr: std::net::SocketAddr,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
}

#[cfg_attr(any, feature = "usdt-probes", feature = "otel-tracing")]
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ResponseInfo {
    pub id: String,
    pub local_addr: std::net::SocketAddr,
    pub remote_addr: std::net::SocketAddr,
    pub status_code: u16,
    pub message: String,
}
*/
