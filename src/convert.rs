//! JSON in, protobuf's well-known types out: an extension's own data (its settings, a command's params and result,
//! a board's items) is free-form on the wire (`Struct`, `Value`), and `serde_json::Value` in the code.

use buffa_types::google::protobuf::{Struct, Value as ProtoValue};
use serde_json::Value;

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
    serde_json::to_value(s).unwrap_or_else(|_| Value::Object(Default::default()))
}

/// Any JSON as a protobuf `Value`.
pub fn to_proto(value: &Value) -> ProtoValue {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| ProtoValue::null())
}

/// A protobuf `Value` as JSON.
pub fn from_proto(v: &ProtoValue) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}
