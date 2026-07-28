//! Secret and raw-payload exclusion for local diagnostics.

use std::fmt;

use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::FieldName;

pub const REDACTED: &str = "[REDACTED]";
const RAW_PAYLOAD_REDACTED: &str = "[RAW_PAYLOAD_REDACTED]";

#[derive(Clone, Copy)]
pub struct SecretValue<'a>(&'a str);

impl<'a> SecretValue<'a> {
    pub const fn new(value: &'a str) -> Self {
        Self(value)
    }
}

impl fmt::Display for SecretValue<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0;
        formatter.write_str(REDACTED)
    }
}

impl fmt::Debug for SecretValue<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for SecretValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(REDACTED)
    }
}

#[derive(Clone, Copy)]
pub struct RawPayload<'a>(&'a [u8]);

impl<'a> RawPayload<'a> {
    pub const fn new(value: &'a [u8]) -> Self {
        Self(value)
    }
}

impl fmt::Display for RawPayload<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.0;
        formatter.write_str(RAW_PAYLOAD_REDACTED)
    }
}

impl fmt::Debug for RawPayload<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Serialize for RawPayload<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(RAW_PAYLOAD_REDACTED)
    }
}

#[derive(Clone, Copy, Default)]
pub struct Redactor;

impl Redactor {
    pub fn field(self, key: &str, value: &str) -> String {
        if sensitive_key(key) || sensitive_value(value) {
            REDACTED.to_owned()
        } else {
            sanitize(value)
        }
    }

    pub fn json(self, value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        if sensitive_key(key) {
                            (key.clone(), Value::String(REDACTED.to_owned()))
                        } else {
                            (key.clone(), self.json(value))
                        }
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .take(1_024)
                    .map(|value| self.json(value))
                    .collect(),
            ),
            Value::String(value) => Value::String(self.field("value", value)),
            value => value.clone(),
        }
    }

    pub fn structured_event(self, value: &Value) -> Value {
        let Value::Object(map) = value else {
            return Value::Object(Default::default());
        };
        Value::Object(
            map.iter()
                .filter_map(|(key, value)| {
                    let field = FieldName::parse(key)?;
                    Some((
                        key.clone(),
                        if field == FieldName::Spans {
                            self.span_names(value)
                        } else {
                            self.event_field(field, key, value)
                        },
                    ))
                })
                .collect(),
        )
    }

    fn event_field(self, field: FieldName, key: &str, value: &Value) -> Value {
        let numeric = matches!(
            field,
            FieldName::Sequence | FieldName::QueueDepth | FieldName::DurationMicros
        );
        match (numeric, value) {
            (true, Value::Number(value)) if value.is_i64() || value.is_u64() => {
                Value::Number(value.clone())
            }
            (false, Value::String(value)) => Value::String(self.field(key, value)),
            _ => Value::String(REDACTED.to_owned()),
        }
    }

    fn span_names(self, value: &Value) -> Value {
        let Value::Array(values) = value else {
            return Value::String(REDACTED.to_owned());
        };
        Value::Array(
            values
                .iter()
                .take(32)
                .map(|value| match value {
                    Value::String(value) => Value::String(self.field("spans", value)),
                    _ => Value::String(REDACTED.to_owned()),
                })
                .collect(),
        )
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "authorization",
        "cookie",
        "api_key",
        "private_key",
        "credential",
        "payload",
        "request_body",
        "response_body",
        "raw_payload",
        "raw_news",
        "news_text",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

fn sensitive_value(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("bearer ")
        || lowercase.contains("password=")
        || lowercase.contains("secret=")
        || lowercase.contains("token=")
        || lowercase.contains("api_key=")
        || lowercase.contains("session_secret")
        || lowercase.starts_with("keychain://")
        || contains_json_container(value)
}

fn contains_json_container(value: &str) -> bool {
    serde_json::from_str::<Value>(value)
        .ok()
        .is_some_and(|parsed| matches!(parsed, Value::Object(_) | Value::Array(_)))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(4_096)
        .collect()
}
