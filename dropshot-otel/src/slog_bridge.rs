// Copyright 2026 Oxide Computer Company
//! A `tracing`-to-`slog` bridge.
//!
//! Dropshot applications typically log through `slog`.  Once a global
//! `tracing` subscriber is installed, `tracing` events emitted by
//! instrumented libraries would otherwise vanish; [`SlogBridge`] forwards
//! them (message, level, and structured fields) to a `slog` logger so all
//! logging lands in one place.  Spans are not forwarded — they are the
//! OpenTelemetry layer's business.

use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A [`Layer`] that forwards `tracing` events to a [`slog::Logger`].
#[derive(Debug)]
pub struct SlogBridge {
    logger: slog::Logger,
}

impl SlogBridge {
    pub fn new(logger: slog::Logger) -> Self {
        Self { logger }
    }
}

impl<S: Subscriber> Layer<S> for SlogBridge {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.unwrap_or_default();
        let kv = FieldKV(visitor.fields);

        match *event.metadata().level() {
            tracing::Level::TRACE => {
                slog::trace!(self.logger, "{}", message; kv)
            }
            tracing::Level::DEBUG => {
                slog::debug!(self.logger, "{}", message; kv)
            }
            tracing::Level::INFO => slog::info!(self.logger, "{}", message; kv),
            tracing::Level::WARN => slog::warn!(self.logger, "{}", message; kv),
            tracing::Level::ERROR => {
                slog::error!(self.logger, "{}", message; kv)
            }
        }
    }
}

/// A field value captured from a `tracing` event.
#[derive(Debug, Clone)]
enum FieldValue {
    Str(String),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
}

/// Extracts the message and structured fields from a `tracing` event.
#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<(String, FieldValue)>,
}

impl EventVisitor {
    fn push(&mut self, field: &tracing::field::Field, value: FieldValue) {
        self.fields.push((field.name().to_string(), value));
    }
}

impl tracing::field::Visit for EventVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.push(field, FieldValue::Str(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field, FieldValue::I64(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field, FieldValue::U64(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field, FieldValue::F64(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field, FieldValue::Bool(value));
    }

    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        if field.name() == "message" {
            self.message = Some(format!("{:?}", value));
        } else {
            self.push(field, FieldValue::Str(format!("{:?}", value)));
        }
    }
}

/// Adapts captured event fields to slog's key-value serialization.
struct FieldKV(Vec<(String, FieldValue)>);

impl slog::KV for FieldKV {
    fn serialize(
        &self,
        _record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        for (key, value) in &self.0 {
            let key = slog::Key::from(key.clone());
            match value {
                FieldValue::Str(v) => serializer.emit_str(key, v)?,
                FieldValue::I64(v) => serializer.emit_i64(key, *v)?,
                FieldValue::U64(v) => serializer.emit_u64(key, *v)?,
                FieldValue::F64(v) => serializer.emit_f64(key, *v)?,
                FieldValue::Bool(v) => serializer.emit_bool(key, *v)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::SlogBridge;
    use slog::Drain;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::layer::SubscriberExt;

    /// A `Write` implementation backed by a shared buffer, so the test can
    /// read back what slog-json wrote.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_bridge_forwards_events_with_fields() {
        let buf = SharedBuf::default();
        let drain = Mutex::new(slog_json::Json::default(buf.clone())).fuse();
        let logger = slog::Logger::root(drain, slog::o!());

        let subscriber =
            tracing_subscriber::registry().with(SlogBridge::new(logger));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                string_field = "value",
                int_field = -3i64,
                uint_field = 7u64,
                float_field = 0.5,
                bool_field = true,
                "hello from tracing"
            );
        });

        let bytes = buf.0.lock().unwrap().clone();
        let line = String::from_utf8(bytes).unwrap();
        let record: serde_json::Value =
            serde_json::from_str(line.lines().next().unwrap()).unwrap();
        assert_eq!(record["msg"], "hello from tracing");
        assert_eq!(record["level"], "INFO");
        assert_eq!(record["string_field"], "value");
        assert_eq!(record["int_field"], -3);
        assert_eq!(record["uint_field"], 7);
        assert_eq!(record["float_field"], 0.5);
        assert_eq!(record["bool_field"], true);
    }
}
