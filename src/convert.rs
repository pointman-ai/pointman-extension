//! JSON in, protobuf's well-known types out: an extension's own data (its settings, a command's params and result,
//! a board's items) is free-form on the wire (`Struct`, `Value`), and `serde_json::Value` in the code.
//!
//! Protobuf's `Value` has one kind of number, a double, so `4` comes off the wire as `4.0`. Coming out of a `Struct`
//! or `Value` here, a double that is a whole number (below 2^53) is the integer again, so `as_u64()`, `== json!(4)`
//! and the rest work as they did when the wire was plain JSON.

use buffa_types::google::protobuf::{Struct, Value as ProtoValue};
use serde_json::{Number, Value};

const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53

/// A JSON object as a `Struct`; anything else is an empty one.
pub fn to_struct(value: &Value) -> Struct {
    if value.is_object() {
        serde_json::from_value(value.clone()).unwrap_or_default()
    } else {
        Struct::default()
    }
}

/// A `Struct` as a JSON object.
pub fn from_struct(s: &Struct) -> Value {
    whole_numbers(serde_json::to_value(s).unwrap_or_else(|_| Value::Object(Default::default())))
}

/// Any JSON as a protobuf `Value`.
pub fn to_proto(value: &Value) -> ProtoValue {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| ProtoValue::null())
}

/// A protobuf `Value` as JSON.
pub fn from_proto(v: &ProtoValue) -> Value {
    whole_numbers(serde_json::to_value(v).unwrap_or(Value::Null))
}

/// JSON with each whole-number double as an integer.
pub fn whole_numbers(value: Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < EXACT => Value::Number(Number::from(f as i64)),
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(whole_numbers).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, whole_numbers(v))).collect()),
        other => other,
    }
}
