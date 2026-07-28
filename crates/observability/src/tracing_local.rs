//! Structured local tracing with field-aware redaction.

use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Map, Value};
use tracing::{
    Dispatch, Event, Id, Level, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Record},
};
use tracing_subscriber::{
    Registry,
    layer::{Context, Layer, SubscriberExt},
    registry::LookupSpan,
};

use crate::{FieldName, LocalJsonLog, ObservabilityError, Redactor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LocalLogLevel {
    const fn allows(self, level: &Level) -> bool {
        let maximum = match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
            Self::Trace => 4,
        };
        let observed = match *level {
            Level::ERROR => 0,
            Level::WARN => 1,
            Level::INFO => 2,
            Level::DEBUG => 3,
            Level::TRACE => 4,
        };
        observed <= maximum
    }
}

pub struct TestObservability {
    dispatch: Dispatch,
    events: Arc<Mutex<Vec<Value>>>,
}

impl TestObservability {
    pub fn with_default<T>(&self, operation: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, operation)
    }

    pub fn finish(self) -> Result<String, ObservabilityError> {
        let events = self
            .events
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?;
        let mut output = String::new();
        for event in events.iter() {
            output.push_str(&serde_json::to_string(event)?);
            output.push('\n');
        }
        Ok(output)
    }
}

pub fn init_test_observability() -> TestObservability {
    let events = Arc::new(Mutex::new(Vec::new()));
    let failures = Arc::new(AtomicU64::new(0));
    let subscriber = Registry::default().with(LocalEventLayer {
        sink: EventSink::Buffer(Arc::clone(&events)),
        redactor: Redactor,
        level: LocalLogLevel::Trace,
        failures,
    });
    TestObservability {
        dispatch: Dispatch::new(subscriber),
        events,
    }
}

pub struct LocalTracing {
    dispatch: Dispatch,
    log: LocalJsonLog,
    failures: Arc<AtomicU64>,
}

impl LocalTracing {
    pub fn with_default<T>(&self, operation: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, operation)
    }

    pub fn sync(&self) -> Result<(), ObservabilityError> {
        self.log.sync()?;
        let failures = self.failures.load(Ordering::Acquire);
        if failures == 0 {
            Ok(())
        } else {
            Err(ObservabilityError::LogWriteFailures { count: failures })
        }
    }

    pub fn shutdown(self) -> Result<(), ObservabilityError> {
        self.sync()
    }
}

pub fn init_local_tracing(log: LocalJsonLog, level: LocalLogLevel) -> LocalTracing {
    let failures = Arc::new(AtomicU64::new(0));
    let subscriber = Registry::default().with(LocalEventLayer {
        sink: EventSink::Log(log.clone()),
        redactor: Redactor,
        level,
        failures: Arc::clone(&failures),
    });
    LocalTracing {
        dispatch: Dispatch::new(subscriber),
        log,
        failures,
    }
}

enum EventSink {
    Buffer(Arc<Mutex<Vec<Value>>>),
    Log(LocalJsonLog),
}

struct LocalEventLayer {
    sink: EventSink,
    redactor: Redactor,
    level: LocalLogLevel,
    failures: Arc<AtomicU64>,
}

impl<S> Layer<S> for LocalEventLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn enabled(&self, metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        self.level.allows(metadata.level())
    }

    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            self.failures.fetch_add(1, Ordering::AcqRel);
            return;
        };
        let mut visitor = JsonVisitor {
            fields: Map::new(),
            redactor: self.redactor,
        };
        attributes.record(&mut visitor);
        span.extensions_mut().insert(SpanFields(visitor.fields));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            self.failures.fetch_add(1, Ordering::AcqRel);
            return;
        };
        let mut visitor = JsonVisitor {
            fields: Map::new(),
            redactor: self.redactor,
        };
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(fields) = extensions.get_mut::<SpanFields>() {
            fields.0.extend(visitor.fields);
        } else {
            extensions.insert(SpanFields(visitor.fields));
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = JsonVisitor {
            fields: Map::new(),
            redactor: self.redactor,
        };
        event.record(&mut visitor);
        visitor.fields.insert(
            "level".to_owned(),
            Value::String(metadata.level().as_str().to_ascii_lowercase()),
        );
        visitor
            .fields
            .entry("event".to_owned())
            .or_insert_with(|| Value::String(metadata.name().to_owned()));
        visitor.fields.insert(
            "target".to_owned(),
            Value::String(metadata.target().to_owned()),
        );
        if let Some(span) = context.lookup_current() {
            let scope = span.scope().from_root().collect::<Vec<_>>();
            let spans = scope
                .iter()
                .map(|span| Value::String(span.metadata().name().to_owned()))
                .collect();
            let mut inherited_fields = Map::new();
            for span in scope {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    for (name, value) in &fields.0 {
                        inherited_fields.insert(name.clone(), value.clone());
                    }
                }
            }
            for (name, value) in inherited_fields {
                visitor.fields.entry(name).or_insert(value);
            }
            visitor
                .fields
                .insert("spans".to_owned(), Value::Array(spans));
        }

        let event = Value::Object(visitor.fields);
        let result = match &self.sink {
            EventSink::Buffer(events) => events
                .lock()
                .map(|mut events| events.push(event))
                .map_err(|_| ()),
            EventSink::Log(log) => log.write_event(&event).map_err(|_| ()),
        };
        if result.is_err() {
            self.failures.fetch_add(1, Ordering::AcqRel);
        }
    }
}

struct SpanFields(Map<String, Value>);

struct JsonVisitor {
    fields: Map<String, Value>,
    redactor: Redactor,
}

impl JsonVisitor {
    fn record_text(&mut self, field: &Field, value: &str) {
        if FieldName::parse(field.name()).is_none() {
            return;
        }
        self.fields.insert(
            field.name().to_owned(),
            Value::String(self.redactor.field(field.name(), value)),
        );
    }

    fn record_value(&mut self, field: &Field, value: Value) {
        match FieldName::parse(field.name()) {
            Some(
                FieldName::Sequence
                | FieldName::QueueDepth
                | FieldName::DurationMicros
                | FieldName::PendingWalCompressionJobs,
            ) if value.is_i64() || value.is_u64() => {
                self.fields.insert(field.name().to_owned(), value);
            }
            Some(_) => {
                self.fields.insert(
                    field.name().to_owned(),
                    Value::String(crate::REDACTED.to_owned()),
                );
            }
            None => {}
        }
    }
}

impl Visit for JsonVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, Value::Number(value.into()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if let Some(value) = serde_json::Number::from_f64(value) {
            self.record_value(field, Value::Number(value));
        } else {
            self.record_text(field, "non-finite");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_text(field, value);
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let _ = value;
        self.record_text(field, crate::REDACTED);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = value;
        self.record_text(field, crate::REDACTED);
    }
}
