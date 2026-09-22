// Copyright 2026 Oxide Computer Company
//! Internal per-request instrumentation.
//!
//! The request path reports a small, fixed set of events: a request starts,
//! and then it either completes with a status code, completes with an error,
//! or is cancelled because the client disconnected.  This module gives those
//! events a single seam so that the request-handling code in `server.rs`
//! stays free of feature gates:
//! [`RequestInstrumentation`] fires the USDT probes (under the
//! `usdt-probes` feature) for each event, and without that feature it is a
//! zero-sized type whose methods are inlineable no-ops.

#[cfg(feature = "usdt-probes")]
use crate::dtrace::probes;
use crate::server::{DropshotState, ServerContext};
use hyper::Request;
use hyper::body::Incoming;
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
}

#[cfg_attr(not(feature = "usdt-probes"), allow(unused_variables))]
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
    }

    /// Reports that the client disconnected before a response was returned.
    pub fn disconnected(&self) {
        #[cfg(feature = "usdt-probes")]
        probes::request__done!(|| {
            crate::dtrace::ResponseInfo {
                id: self.request_id.to_string(),
                local_addr: self.local_addr,
                remote_addr: self.remote_addr,
                // 499 is a non-standard code popularized by nginx to mean
                // "client disconnected".
                status_code: 499,
                message: String::from(
                    "client disconnected before response returned",
                ),
            }
        });
    }
}
