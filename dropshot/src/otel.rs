// Copyright 2024 Oxide Computer Company
//! Opentelemetry tracing support

#[cfg(feature = "otel-tracing")]
use opentelemetry::{global, trace::Tracer, trace::Span};
#[cfg(feature = "otel-tracing")]
use opentelemetry_semantic_conventions::trace;
#[cfg(feature = "otel-tracing")]
use opentelemetry_http::HeaderExtractor;

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

pub trait TraceRequest {
    fn trace_request(&mut self, request: &hyper::Request<hyper::body::Incoming>);
    fn trace_error(&mut self, error: &dyn std::error::Error);
}

impl TraceRequest for opentelemetry::global::BoxedSpan {
    fn trace_request(&mut self, request: &hyper::Request<hyper::body::Incoming>){
        let attributes: [(opentelemetry::Key, opentelemetry::Value);4] = [
            ("http.method".into(), request.method().to_string().into()),
            ("http.path".into(), request.uri().path().to_string().into()),
            ("http.uri".into(), request.uri().to_string().into()),
            ("http.uri".into(), request.uri().to_string().into()),
        ];
        self.add_event("incoming request", attributes.map(|(k,v)| opentelemetry::KeyValue::new(k,v)).to_vec());
/*
        //opentelemetry::KeyValue::new("http.version".to_string(),request.version()),
        //opentelemetry::KeyValue::new("http.headers.accept".to_string(),request.headers()["accept"]),
        http.version = format!("{:#?}",request.version()),
        http.headers.accept = format!("{:#?}", request.headers()["accept"]),
        http.headers.host = format!("{:#?}", request.headers()["host"]),
        http.headers.user_agent = format!("{:#?}", request.headers()["user-agent"]),
            id: request_id.clone(),
            local_addr: server.local_addr,
            remote_addr,
            method: request.method().to_string(),
            path: uri.path().to_string(),
            query: uri.query().map(|x| x.to_string()),
*/
    }
    fn trace_error(&mut self, error: &dyn std::error::Error) {
        self.record_error(&error)
    }
}

pub trait TraceResponse<T> {
    fn trace_response(&mut self, response: &hyper::Response<T>);
}

impl<T> TraceResponse<T> for opentelemetry::global::BoxedSpan {
    fn trace_response(&mut self, result: &hyper::Response<T>) {
        self.set_attribute(opentelemetry::KeyValue::new(trace::HTTP_RESPONSE_STATUS_CODE, i64::from(result.status().as_u16())));
    }
}
