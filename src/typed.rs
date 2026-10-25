//! An extension's own messages as Rust types: its proto (`proto/<id>/v1/<id>.proto`, Pointman's docs/dev/extensions.md,
//! "Best practice: your own .proto") built by [`proto::build`](crate::proto) into buffa's code, with a [`Typed`] for
//! each of its messages and a [`Command`] for each `<Verb>Params` that has its `<Verb>Result`. A typed handler takes
//! its params' message and gives its result's ([`Extension::typed_command`](crate::Extension::typed_command)), and
//! [`Handle::typed_settings`](crate::Handle::typed_settings) reads `message Settings`.
//!
//! The JSON is the manifest's, as `pointman-extension manifest write` writes its schemas: a field by its name as the
//! proto has it (`made_for_kids`), an enum by its value's name in lower case without the enum's prefix (`public` for
//! `PRIVACY_PUBLIC`), and a 64-bit integer as a number. [`Shapes`] says where those are, from the extension's
//! descriptors, and [`from_value`] and [`to_value`] go between that JSON and protobuf's standard JSON, which buffa's
//! types read and write.

use std::collections::HashMap;

use anyhow::{anyhow, Context as _};
use buffa::Message as _;
use buffa_descriptor::generated::descriptor::field_descriptor_proto::{Label, Type};
use buffa_descriptor::generated::descriptor::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

/// A message of the extension's own proto, as [`proto::build`](crate::proto) makes it.
pub trait Typed: Serialize + DeserializeOwned {
    /// Its full name: `youtube.v1.UploadParams`.
    const NAME: &'static str;
    /// What the extension's descriptors say about its messages' JSON.
    fn shapes() -> &'static Shapes;
}

/// A command's params message (`UploadParams` for `youtube.upload`), with its kind and its result's message.
pub trait Command: Typed {
    /// The command's kind in extension.toml: `youtube.upload`.
    const KIND: &'static str;
    /// `UploadResult`.
    type Result: Typed;
}

/// Where an extension's messages hold enums, 64-bit integers and messages: what [`from_value`] and [`to_value`] need
/// to change between the manifest's JSON and protobuf's.
#[derive(Debug, Default)]
pub struct Shapes {
    messages: HashMap<String, Vec<Field>>,
    enums: HashMap<String, Vec<(String, String)>>,
}

#[derive(Debug)]
struct Field {
    name: String,
    kind: Kind,
    /// repeated: a list, or a map's values when `map` is set
    repeated: bool,
    map: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Enum(String),
    Message(String),
    Int64,
    Other,
}

impl Shapes {
    /// From a FileDescriptorSet's bytes (what `proto::build` writes beside buffa's code).
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Shapes> {
        let set = FileDescriptorSet::decode_from_slice(bytes).context("the extension's descriptors")?;
        let mut shapes = Shapes::default();
        for file in &set.file {
            let package = file.package.clone().unwrap_or_default();
            let scope = if package.is_empty() { String::new() } else { format!(".{package}") };
            for e in &file.enum_type {
                shapes.add_enum(&scope, e);
            }
            for m in &file.message_type {
                shapes.add_message(&scope, m);
            }
        }
        // a map's entry is its key and value: the value is what the map's field holds
        let entries: HashMap<String, Kind> = set
            .file
            .iter()
            .flat_map(|f| {
                let scope = f.package.as_deref().map(|p| format!(".{p}")).unwrap_or_default();
                entries(&scope, &f.message_type)
            })
            .collect();
        for fields in shapes.messages.values_mut() {
            for f in fields {
                if let Kind::Message(name) = &f.kind {
                    if let Some(value) = entries.get(name) {
                        f.kind = value.clone();
                        f.map = true;
                    }
                }
            }
        }
        Ok(shapes)
    }

    fn add_enum(&mut self, scope: &str, e: &EnumDescriptorProto) {
        let name = e.name.clone().unwrap_or_default();
        let prefix = enum_prefix(&name);
        let values = e.value.iter().filter_map(|v| v.name.clone()).map(|v| (enum_word(&prefix, &v), v)).collect();
        self.enums.insert(format!("{scope}.{name}"), values);
    }

    fn add_message(&mut self, scope: &str, m: &DescriptorProto) {
        let full = format!("{scope}.{}", m.name.as_deref().unwrap_or_default());
        for e in &m.enum_type {
            self.add_enum(&full, e);
        }
        for n in &m.nested_type {
            self.add_message(&full, n);
        }
        let fields = m
            .field
            .iter()
            .map(|f| Field {
                name: f.name.clone().unwrap_or_default(),
                kind: kind_of(f),
                repeated: f.label == Some(Label::LABEL_REPEATED),
                map: false,
            })
            .collect();
        self.messages.insert(full, fields);
    }

    /// The manifest's JSON as protobuf's, for message `name` (`youtube.v1.UploadParams`): each enum's word as its
    /// value's name. A word that isn't one of the enum's is refused, saying which it takes, and so is a field the
    /// message doesn't have.
    pub fn to_proto(&self, name: &str, value: &mut Value) -> anyhow::Result<()> {
        self.walk(&format!(".{name}"), value, true, "")
    }

    /// Protobuf's JSON as the manifest's, for message `name`: each enum's value as its word, each 64-bit integer as a
    /// number.
    pub fn from_proto(&self, name: &str, value: &mut Value) {
        let _ = self.walk(&format!(".{name}"), value, false, "");
    }

    fn walk(&self, message: &str, value: &mut Value, inward: bool, at: &str) -> anyhow::Result<()> {
        let (Some(fields), Value::Object(object)) = (self.messages.get(message), value) else {
            return Ok(());
        };
        if inward {
            // a message is closed, as its schema says (`additionalProperties = false`): a mistyped name is refused
            if let Some(key) = object.keys().find(|k| !fields.iter().any(|f| &f.name == *k)) {
                let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                let at = if at.is_empty() { key.clone() } else { format!("{at}.{key}") };
                return Err(anyhow!("{at} isn't a field: it takes {}", names.join(", ")));
            }
        }
        for f in fields {
            let Some(v) = object.get_mut(&f.name) else { continue };
            let path = if at.is_empty() { f.name.clone() } else { format!("{at}.{}", f.name) };
            match (f.repeated, f.map, v) {
                (true, true, Value::Object(map)) => {
                    for (key, item) in map.iter_mut() {
                        self.one(&f.kind, item, inward, &format!("{path}.{key}"))?;
                    }
                }
                (true, false, Value::Array(list)) => {
                    for (i, item) in list.iter_mut().enumerate() {
                        self.one(&f.kind, item, inward, &format!("{path}[{i}]"))?;
                    }
                }
                (_, _, v) => self.one(&f.kind, v, inward, &path)?,
            }
        }
        Ok(())
    }

    fn one(&self, kind: &Kind, value: &mut Value, inward: bool, at: &str) -> anyhow::Result<()> {
        match kind {
            Kind::Message(name) => self.walk(name, value, inward, at),
            Kind::Enum(name) => {
                let (Some(values), Value::String(text)) = (self.enums.get(name), &*value) else {
                    return Ok(());
                };
                let found = if inward {
                    values.iter().find(|(word, proto)| word == text || proto == text).map(|(_, proto)| proto)
                } else {
                    values.iter().find(|(_, proto)| proto == text).map(|(word, _)| word)
                };
                match found {
                    Some(found) => *value = Value::String(found.clone()),
                    None if inward => {
                        let words: Vec<&str> =
                            values.iter().map(|(w, _)| w.as_str()).filter(|w| *w != "unspecified").collect();
                        return Err(anyhow!("{at}: {value} isn't one of {}", words.join(", ")));
                    }
                    None => {}
                }
                Ok(())
            }
            Kind::Int64 if !inward => {
                if let Value::String(text) = &*value {
                    if let Ok(n) = text.parse::<i64>() {
                        *value = n.into();
                    } else if let Ok(n) = text.parse::<u64>() {
                        *value = n.into();
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn entries(scope: &str, messages: &[DescriptorProto]) -> Vec<(String, Kind)> {
    let mut out = Vec::new();
    for m in messages {
        let full = format!("{scope}.{}", m.name.as_deref().unwrap_or_default());
        if m.options.as_option().and_then(|o| o.map_entry).unwrap_or(false) {
            if let Some(value) = m.field.iter().find(|f| f.number == Some(2)) {
                out.push((full.clone(), kind_of(value)));
            }
        }
        out.extend(entries(&full, &m.nested_type));
    }
    out
}

fn kind_of(f: &FieldDescriptorProto) -> Kind {
    let type_name = f.type_name.clone().unwrap_or_default();
    match f.r#type {
        Some(Type::TYPE_ENUM) => Kind::Enum(type_name),
        Some(Type::TYPE_MESSAGE) => Kind::Message(type_name),
        Some(Type::TYPE_INT64 | Type::TYPE_UINT64 | Type::TYPE_SINT64 | Type::TYPE_FIXED64 | Type::TYPE_SFIXED64) => {
            Kind::Int64
        }
        _ => Kind::Other,
    }
}

/// The prefix every value of enum `name` starts with: `PRIVACY_` for `Privacy`, `ON_EXIT_` for `OnExit`.
pub fn enum_prefix(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_uppercase());
    }
    out.push('_');
    out
}

/// A value's word in the manifest's JSON: `PRIVACY_PUBLIC` is `public`.
pub fn enum_word(prefix: &str, value: &str) -> String {
    value.strip_prefix(prefix).unwrap_or(value).to_lowercase()
}

/// A message from the manifest's JSON (a command's params, the settings).
pub fn from_value<T: Typed>(mut value: Value) -> anyhow::Result<T> {
    T::shapes().to_proto(T::NAME, &mut value)?;
    crate::from_json(value).with_context(|| format!("not a {}", T::NAME))
}

/// A message as the manifest's JSON (a command's result).
pub fn to_value<T: Typed>(message: &T) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(message)?;
    T::shapes().from_proto(T::NAME, &mut value);
    Ok(value)
}

/// `given` with the defaults of `schema`'s properties where it has none, after checking it has each of the schema's
/// `required`: what a typed handler gets.
pub(crate) fn with_schema(schema: &Value, given: &Value) -> anyhow::Result<Value> {
    let mut out: Map<String, Value> = given.as_object().cloned().unwrap_or_default();
    if let Some(props) = schema["properties"].as_object() {
        for (name, prop) in props {
            if let (false, Some(default)) = (out.contains_key(name), prop.get("default")) {
                out.insert(name.clone(), default.clone());
            }
        }
    }
    for name in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if out.get(name).is_none_or(Value::is_null) {
            return Err(anyhow!("{name} is missing"));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_words() {
        assert_eq!(enum_prefix("Privacy"), "PRIVACY_");
        assert_eq!(enum_prefix("MadeForKids"), "MADE_FOR_KIDS_");
        assert_eq!(enum_word("MADE_FOR_KIDS_", "MADE_FOR_KIDS_NOT_SURE"), "not_sure");
        assert_eq!(enum_word("PRIVACY_", "OTHER"), "other");
    }

    #[test]
    fn a_handler_gets_the_schemas_defaults_and_its_required() {
        let schema = json!({"type": "object", "properties": {
            "name": {"type": "string"}, "greeting": {"type": "string", "default": "Hello"},
        }, "required": ["name"]});
        assert_eq!(with_schema(&schema, &json!({"name": "Ada"})).unwrap(), json!({"name": "Ada", "greeting": "Hello"}));
        assert_eq!(
            with_schema(&schema, &json!({"name": "Ada", "greeting": "Hi"})).unwrap(),
            json!({"name": "Ada", "greeting": "Hi"})
        );
        assert_eq!(with_schema(&schema, &json!({})).unwrap_err().to_string(), "name is missing");
        assert_eq!(with_schema(&schema, &json!({"name": null})).unwrap_err().to_string(), "name is missing");
    }
}
