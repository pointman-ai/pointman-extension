//! A message as the JSON Schema a manifest holds: see the module above for what each part of the proto becomes.

use std::collections::HashMap;

use anyhow::{anyhow, bail};
use protox::prost_reflect::{
    DescriptorPool, DynamicMessage, EnumDescriptor, ExtensionDescriptor, FieldDescriptor, FileDescriptor, Kind,
    MessageDescriptor, Value as PValue,
};
use serde_json::Value;

use crate::typed::{enum_prefix, enum_word};

/// JSON with its keys in order: a schema reads as the proto does (its fields in their order, `type` first), and
/// TOML keeps that order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Node>),
    Obj(Vec<(String, Node)>),
}

impl Node {
    pub(crate) fn json(&self) -> Value {
        match self {
            Node::Str(s) => Value::String(s.clone()),
            Node::Int(i) => Value::from(*i),
            Node::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Node::Bool(b) => Value::Bool(*b),
            Node::List(l) => Value::Array(l.iter().map(Node::json).collect()),
            Node::Obj(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), v.json())).collect()),
        }
    }
}

fn s(text: impl Into<String>) -> Node {
    Node::Str(text.into())
}

/// Sets `key`, where it was or at the end.
fn set(obj: &mut Vec<(String, Node)>, key: &str, value: Node) {
    match obj.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value,
        None => obj.push((key.to_string(), value)),
    }
}

fn get<'a>(obj: &'a mut [(String, Node)], key: &str) -> Option<&'a mut Node> {
    obj.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A description, after `type`.
fn describe(obj: &mut Vec<(String, Node)>, doc: String) {
    match obj.iter().position(|(k, _)| k == "description") {
        Some(i) => obj[i].1 = Node::Str(doc),
        None => {
            let at = obj.iter().position(|(k, _)| k == "type").map_or(0, |i| i + 1);
            obj.insert(at, ("description".into(), Node::Str(doc)));
        }
    }
}

pub(crate) struct Schemas {
    field_rules: Option<ExtensionDescriptor>,
    message_rules: Option<ExtensionDescriptor>,
    oneof_rules: Option<ExtensionDescriptor>,
    setting: Option<ExtensionDescriptor>,
    comments: HashMap<String, HashMap<Vec<i32>, String>>,
    /// the messages being written, inner last: one that holds itself can't be said inline
    stack: Vec<String>,
}

impl Schemas {
    pub(crate) fn new(pool: &DescriptorPool) -> Self {
        Schemas {
            field_rules: pool.get_extension_by_name("buf.validate.field"),
            message_rules: pool.get_extension_by_name("buf.validate.message"),
            oneof_rules: pool.get_extension_by_name("buf.validate.oneof"),
            setting: pool.get_extension_by_name("pointman.v1.setting"),
            comments: HashMap::new(),
            stack: Vec::new(),
        }
    }

    /// An element's comment: the one above it, else the one after it on its line, in one line (as core reads its own
    /// protos' comments).
    fn doc(&mut self, file: &FileDescriptor, path: &[i32]) -> Option<String> {
        let notes = self.comments.entry(file.name().to_string()).or_insert_with(|| {
            let info = file.file_descriptor_proto().source_code_info.clone().unwrap_or_default();
            info.location
                .iter()
                .filter_map(|loc| {
                    let text = loc.leading_comments.as_deref().or(loc.trailing_comments.as_deref())?;
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    (!text.is_empty()).then(|| (loc.path.clone(), text))
                })
                .collect()
        });
        notes.get(path).cloned()
    }

    pub(crate) fn message(&mut self, m: &MessageDescriptor) -> anyhow::Result<Node> {
        if self.stack.iter().any(|n| n == m.full_name()) {
            bail!("{} holds itself, which a manifest's schema can't say", m.full_name());
        }
        self.stack.push(m.full_name().to_string());
        let mut obj = vec![("type".to_string(), s("object"))];
        if let Some(doc) = self.doc(&m.parent_file(), m.path()) {
            describe(&mut obj, doc);
        }
        if let Some(rules) = self.extension(&m.options(), &self.message_rules.clone()) {
            for (fd, _) in rules.fields() {
                if fd.name() != "disabled" {
                    bail!("{}: (buf.validate.message).{} has no JSON Schema keyword", m.full_name(), fd.name());
                }
            }
        }
        let mut props = Vec::new();
        let mut required = Vec::new();
        for f in m.fields() {
            let (schema, needed) = self.field(&f)?;
            props.push((f.name().to_string(), schema));
            if needed {
                required.push(s(f.name()));
            }
        }
        // none: no table of properties, which TOML wouldn't keep anyway
        if !props.is_empty() {
            obj.push(("properties".into(), Node::Obj(props)));
        }
        if !required.is_empty() {
            obj.push(("required".into(), Node::List(required)));
        }
        // a message is closed: a field it doesn't have is a mistake, refused rather than ignored (inputs are strict)
        obj.push(("additionalProperties".into(), Node::Bool(false)));
        // a oneof buf.validate requires: exactly one of its fields
        let mut groups = Vec::new();
        for o in m.oneofs().filter(|o| !o.is_synthetic()) {
            let Some(rules) = self.extension(&o.options(), &self.oneof_rules.clone()) else { continue };
            for (fd, v) in rules.fields() {
                match fd.name() {
                    "required" if v.as_bool() == Some(true) => groups.push(Node::List(
                        o.fields()
                            .map(|f| Node::Obj(vec![("required".into(), Node::List(vec![s(f.name())]))]))
                            .collect(),
                    )),
                    "required" => {}
                    other => bail!("{}: (buf.validate.oneof).{other} has no JSON Schema keyword", o.full_name()),
                }
            }
        }
        match groups.len() {
            0 => {}
            1 => obj.push(("oneOf".into(), groups.remove(0))),
            _ => obj.push((
                "allOf".into(),
                Node::List(groups.into_iter().map(|g| Node::Obj(vec![("oneOf".into(), g)])).collect()),
            )),
        }
        self.stack.pop();
        Ok(Node::Obj(obj))
    }

    fn extension(&self, options: &DynamicMessage, ext: &Option<ExtensionDescriptor>) -> Option<DynamicMessage> {
        let ext = ext.as_ref()?;
        if !options.has_extension(ext) {
            return None;
        }
        options.get_extension(ext).as_message().cloned()
    }

    /// A field's schema, and whether buf.validate requires it.
    fn field(&mut self, f: &FieldDescriptor) -> anyhow::Result<(Node, bool)> {
        let what = f.full_name().to_string();
        let mut obj = if f.is_map() {
            let Kind::Message(entry) = f.kind() else { unreachable!("a map's entry is a message") };
            let value = self.single(&entry.map_entry_value_field())?;
            vec![("type".to_string(), s("object")), ("additionalProperties".to_string(), value)]
        } else if f.is_list() {
            vec![("type".to_string(), s("array")), ("items".to_string(), self.single(f)?)]
        } else {
            match self.single(f)? {
                Node::Obj(obj) => obj,
                _ => unreachable!("a schema is an object"),
            }
        };
        if let Some(doc) = self.doc(&f.parent_file(), f.path()) {
            describe(&mut obj, doc);
        }
        if let Some(default) = self.default(f)? {
            set(&mut obj, "default", default);
        }
        let mut required = false;
        if let Some(field_rules) = self.extension(&f.options(), &self.field_rules.clone()) {
            required = rules(&what, f, &field_rules, &mut obj, true)?;
        }
        Ok((Node::Obj(obj), required))
    }

    /// The schema of one of a field's values (the field itself, a list's item, a map's value).
    fn single(&mut self, f: &FieldDescriptor) -> anyhow::Result<Node> {
        let ty = |t: &str| vec![("type".to_string(), s(t))];
        Ok(Node::Obj(match f.kind() {
            Kind::Double | Kind::Float => ty("number"),
            Kind::Int32 | Kind::Int64 | Kind::Sint32 | Kind::Sint64 | Kind::Sfixed32 | Kind::Sfixed64 => ty("integer"),
            Kind::Uint32 | Kind::Uint64 | Kind::Fixed32 | Kind::Fixed64 => ty("integer"),
            Kind::Bool => ty("boolean"),
            Kind::String => ty("string"),
            Kind::Bytes => vec![("type".into(), s("string")), ("contentEncoding".into(), s("base64"))],
            Kind::Enum(e) => {
                let mut obj = vec![("type".into(), s("string")), ("enum".into(), Node::List(words(&e)))];
                if let Some(doc) = self.doc(&e.parent_file(), e.path()) {
                    describe(&mut obj, doc);
                }
                obj
            }
            Kind::Message(m) => match well_known(&m) {
                Some(Ok(obj)) => obj,
                Some(Err(e)) => bail!("{}: {e}", f.full_name()),
                None => match self.message(&m)? {
                    Node::Obj(obj) => obj,
                    _ => unreachable!(),
                },
            },
        }))
    }

    /// `(pointman.v1.setting).default`, as the field's JSON.
    fn default(&self, f: &FieldDescriptor) -> anyhow::Result<Option<Node>> {
        let Some(opt) = self.extension(&f.options(), &self.setting) else { return Ok(None) };
        let mut text = None;
        for (fd, v) in opt.fields() {
            match fd.name() {
                "default" => text = v.as_str().map(String::from),
                "kind" => bail!(
                    "{}: (pointman.v1.setting).kind is for Pointman's own app settings; say what it holds in its comment",
                    f.full_name()
                ),
                _ => {}
            }
        }
        let Some(text) = text else { return Ok(None) };
        let what = f.full_name();
        if f.is_map() {
            bail!("{what}: a map has no default");
        }
        if f.is_list() {
            let items = text.split(',').filter(|w| !w.is_empty()).map(|w| scalar(what, &f.kind(), w));
            return Ok(Some(Node::List(items.collect::<anyhow::Result<_>>()?)));
        }
        scalar(what, &f.kind(), &text).map(Some)
    }
}

/// buf.validate's rules on a field (or on a list's items, a map's values) as JSON Schema keywords on `obj`. Returns
/// whether it's required.
fn rules(
    what: &str,
    f: &FieldDescriptor,
    given: &DynamicMessage,
    obj: &mut Vec<(String, Node)>,
    whole: bool,
) -> anyhow::Result<bool> {
    let mut required = false;
    for (fd, v) in given.fields() {
        let name = fd.name();
        match name {
            "required" if whole => required = v.as_bool() == Some(true),
            "ignore" => {}
            "repeated" if whole && f.is_list() => {
                for (rd, rv) in message(v)?.fields() {
                    match rd.name() {
                        "min_items" => set(obj, "minItems", int(rv)?),
                        "max_items" => set(obj, "maxItems", int(rv)?),
                        "unique" => set(obj, "uniqueItems", Node::Bool(rv.as_bool() == Some(true))),
                        "items" => {
                            let Some(Node::Obj(items)) = get(obj, "items") else { unreachable!() };
                            rules(what, f, message(rv)?, items, false)?;
                        }
                        other => bail!(unmapped(what, "repeated", other)),
                    }
                }
            }
            "map" if whole && f.is_map() => {
                let Kind::Message(entry) = f.kind() else { unreachable!() };
                for (rd, rv) in message(v)?.fields() {
                    match rd.name() {
                        "min_pairs" => set(obj, "minProperties", int(rv)?),
                        "max_pairs" => set(obj, "maxProperties", int(rv)?),
                        "values" => {
                            let Some(Node::Obj(values)) = get(obj, "additionalProperties") else { unreachable!() };
                            rules(what, &entry.map_entry_value_field(), message(rv)?, values, false)?;
                        }
                        "keys" => {
                            let mut keys = vec![("type".to_string(), s("string"))];
                            rules(what, &entry.map_entry_key_field(), message(rv)?, &mut keys, false)?;
                            set(obj, "propertyNames", Node::Obj(keys));
                        }
                        other => bail!(unmapped(what, "map", other)),
                    }
                }
            }
            "string" if !f.is_list() || !whole => string_rules(what, message(v)?, obj)?,
            "bool" if !f.is_list() || !whole => {
                for (rd, rv) in message(v)?.fields() {
                    match rd.name() {
                        "const" => set(obj, "const", Node::Bool(rv.as_bool() == Some(true))),
                        "example" => example(obj, Node::Bool(rv.as_bool() == Some(true))),
                        other => bail!(unmapped(what, "bool", other)),
                    }
                }
            }
            "enum" if !f.is_list() || !whole => {
                let Kind::Enum(e) = f.kind() else { bail!("{what}: enum rules on a field that isn't an enum") };
                enum_rules(what, &e, message(v)?, obj)?;
            }
            "int32" | "int64" | "uint32" | "uint64" | "sint32" | "sint64" | "fixed32" | "fixed64" | "sfixed32"
            | "sfixed64" | "float" | "double"
                if !f.is_list() || !whole =>
            {
                number_rules(what, name, message(v)?, obj)?
            }
            other => {
                bail!("{what}: (buf.validate.field).{other} has no JSON Schema keyword, so the manifest can't say it")
            }
        }
    }
    Ok(required)
}

fn unmapped(what: &str, kind: &str, rule: &str) -> String {
    format!("{what}: (buf.validate.field).{kind}.{rule} has no JSON Schema keyword, so the manifest can't say it")
}

fn message(v: &PValue) -> anyhow::Result<&DynamicMessage> {
    v.as_message().ok_or_else(|| anyhow!("a buf.validate rule that isn't a message"))
}

fn int(v: &PValue) -> anyhow::Result<Node> {
    Ok(match v {
        PValue::I32(n) => Node::Int(i64::from(*n)),
        PValue::I64(n) => Node::Int(*n),
        PValue::U32(n) => Node::Int(i64::from(*n)),
        PValue::U64(n) => Node::Int(i64::try_from(*n)?),
        _ => bail!("a buf.validate count that isn't an integer"),
    })
}

fn number(v: &PValue) -> anyhow::Result<Node> {
    Ok(match v {
        PValue::F32(n) => Node::Float(f64::from(*n)),
        PValue::F64(n) => Node::Float(*n),
        other => int(other)?,
    })
}

fn list(v: &PValue, each: impl Fn(&PValue) -> anyhow::Result<Node>) -> anyhow::Result<Node> {
    let items = v.as_list().ok_or_else(|| anyhow!("a buf.validate list that isn't one"))?;
    Ok(Node::List(items.iter().map(each).collect::<anyhow::Result<_>>()?))
}

fn example(obj: &mut Vec<(String, Node)>, value: Node) {
    match get(obj, "examples") {
        Some(Node::List(items)) => items.push(value),
        _ => set(obj, "examples", Node::List(vec![value])),
    }
}

fn string_rules(what: &str, rules: &DynamicMessage, obj: &mut Vec<(String, Node)>) -> anyhow::Result<()> {
    let text = |v: &PValue| -> anyhow::Result<Node> {
        v.as_str().map(s).ok_or_else(|| anyhow!("a buf.validate string rule that isn't text"))
    };
    for (rd, rv) in rules.fields() {
        match rd.name() {
            "min_len" => set(obj, "minLength", int(rv)?),
            "max_len" => set(obj, "maxLength", int(rv)?),
            "len" => {
                set(obj, "minLength", int(rv)?);
                set(obj, "maxLength", int(rv)?);
            }
            "pattern" => set(obj, "pattern", text(rv)?),
            "const" => set(obj, "const", text(rv)?),
            "in" => set(obj, "enum", list(rv, text)?),
            "not_in" => set(obj, "not", Node::Obj(vec![("enum".into(), list(rv, text)?)])),
            "example" => example(obj, text(rv)?),
            format @ ("email" | "uri" | "uuid" | "hostname" | "ipv4" | "ipv6") => {
                if rv.as_bool() == Some(true) {
                    set(obj, "format", s(format));
                }
            }
            bytes @ ("min_bytes" | "max_bytes" | "len_bytes") => bail!(
                "{}; JSON Schema counts characters, so say it with {} and check the bytes in code",
                unmapped(what, "string", bytes),
                bytes.replace("_bytes", "_len")
            ),
            other => bail!(unmapped(what, "string", other)),
        }
    }
    Ok(())
}

fn number_rules(what: &str, kind: &str, rules: &DynamicMessage, obj: &mut Vec<(String, Node)>) -> anyhow::Result<()> {
    let (mut low, mut high) = (None, None);
    let mut bounds = Vec::new();
    for (rd, rv) in rules.fields() {
        let keyword = match rd.name() {
            "const" => "const",
            "gt" => "exclusiveMinimum",
            "gte" => "minimum",
            "lt" => "exclusiveMaximum",
            "lte" => "maximum",
            "in" => {
                set(obj, "enum", list(rv, number)?);
                continue;
            }
            "not_in" => {
                set(obj, "not", Node::Obj(vec![("enum".into(), list(rv, number)?)]));
                continue;
            }
            "example" => {
                example(obj, number(rv)?);
                continue;
            }
            "finite" => continue, // JSON has no NaN or infinity to refuse
            other => bail!(unmapped(what, kind, other)),
        };
        let n = number(rv)?;
        let at = match &n {
            Node::Int(i) => *i as f64,
            Node::Float(f) => *f,
            _ => unreachable!(),
        };
        match keyword {
            "exclusiveMinimum" | "minimum" => low = Some(at),
            "exclusiveMaximum" | "maximum" => high = Some(at),
            _ => {}
        }
        bounds.push((keyword, n));
    }
    // the least before the most, whatever order buf.validate keeps them in
    let rank = ["const", "minimum", "exclusiveMinimum", "maximum", "exclusiveMaximum"];
    bounds.sort_by_key(|(k, _)| rank.iter().position(|r| r == k));
    for (keyword, n) in bounds {
        set(obj, keyword, n);
    }
    if let (Some(low), Some(high)) = (low, high) {
        if low > high {
            // buf.validate reads a lower bound above the upper one as "outside the range"
            bail!("{what}: {kind} rules outside a range have no JSON Schema keyword");
        }
    }
    Ok(())
}

fn enum_rules(
    what: &str,
    e: &EnumDescriptor,
    rules: &DynamicMessage,
    obj: &mut Vec<(String, Node)>,
) -> anyhow::Result<()> {
    let prefix = enum_prefix(e.name());
    let word = |n: i32| -> anyhow::Result<String> {
        let v = e.get_value(n).ok_or_else(|| anyhow!("{what}: {n} isn't a value of {}", e.full_name()))?;
        Ok(enum_word(&prefix, v.name()))
    };
    let numbers = |v: &PValue| -> anyhow::Result<Vec<String>> {
        let items = v.as_list().ok_or_else(|| anyhow!("a buf.validate list that isn't one"))?;
        items.iter().map(|i| word(i.as_i32().unwrap_or_default())).collect()
    };
    for (rd, rv) in rules.fields() {
        match rd.name() {
            "defined_only" => {} // the enum's words are all it takes already
            "const" => set(obj, "const", s(word(rv.as_i32().unwrap_or_default())?)),
            "in" => {
                let keep = numbers(rv)?;
                set(obj, "enum", Node::List(keep.into_iter().map(s).collect()));
            }
            "not_in" => {
                let drop = numbers(rv)?;
                if let Some(Node::List(words)) = get(obj, "enum") {
                    words.retain(|w| !matches!(w, Node::Str(w) if drop.contains(w)));
                }
            }
            "example" => example(obj, s(word(rv.as_i32().unwrap_or_default())?)),
            other => bail!(unmapped(what, "enum", other)),
        }
    }
    Ok(())
}

/// An enum's values as the manifest takes them: `PRIVACY_PUBLIC` is `public`, and its `…_UNSPECIFIED` zero is left out.
fn words(e: &EnumDescriptor) -> Vec<Node> {
    let prefix = enum_prefix(e.name());
    e.values()
        .filter(|v| !(v.number() == 0 && v.name().ends_with("UNSPECIFIED")))
        .map(|v| s(enum_word(&prefix, v.name())))
        .collect()
}

/// A default written as Pointman's settings write it (`pointman config update --values`), as a value of `kind`.
fn scalar(what: &str, kind: &Kind, text: &str) -> anyhow::Result<Node> {
    let wrong = || anyhow!("{what}: its default {text:?} isn't a {}", kind_name(kind));
    Ok(match kind {
        Kind::Bool => match text {
            "true" => Node::Bool(true),
            "false" => Node::Bool(false),
            _ => return Err(wrong()),
        },
        Kind::Double | Kind::Float => Node::Float(text.parse().map_err(|_| wrong())?),
        Kind::String => s(text),
        Kind::Enum(e) => {
            let prefix = enum_prefix(e.name());
            let found = e.values().find(|v| enum_word(&prefix, v.name()) == text || v.name() == text);
            match found {
                Some(v) if !(v.number() == 0 && v.name().ends_with("UNSPECIFIED")) => s(enum_word(&prefix, v.name())),
                _ => {
                    let takes: Vec<String> =
                        words(e).iter().map(|w| w.json().as_str().unwrap_or("").to_string()).collect();
                    bail!("{what}: its default {text:?} isn't one of {}", takes.join(", "))
                }
            }
        }
        Kind::Message(_) | Kind::Bytes => {
            bail!("{what}: a default for a {} isn't something a setting says", kind_name(kind))
        }
        _ => Node::Int(text.parse().map_err(|_| wrong())?),
    })
}

fn kind_name(kind: &Kind) -> &'static str {
    match kind {
        Kind::Bool => "true or false",
        Kind::Double | Kind::Float => "number",
        Kind::String => "string",
        Kind::Enum(_) => "word of its enum",
        Kind::Message(_) => "message",
        Kind::Bytes => "bytes field",
        _ => "whole number",
    }
}

/// google.protobuf's messages as protobuf's JSON has them; None for any other message.
fn well_known(m: &MessageDescriptor) -> Option<Result<Vec<(String, Node)>, String>> {
    let name = m.full_name().strip_prefix("google.protobuf.")?;
    let ty = |t: &str| Ok(vec![("type".to_string(), s(t))]);
    Some(match name {
        "Timestamp" => Ok(vec![("type".into(), s("string")), ("format".into(), s("date-time"))]),
        "Duration" => Ok(vec![("type".into(), s("string")), ("pattern".into(), s(r"^-?[0-9]+(\.[0-9]{1,9})?s$"))]),
        "Struct" | "Empty" => ty("object"),
        "ListValue" => ty("array"),
        "Value" => Ok(vec![]),
        "FieldMask" | "StringValue" => ty("string"),
        "BytesValue" => Ok(vec![("type".into(), s("string")), ("contentEncoding".into(), s("base64"))]),
        "BoolValue" => ty("boolean"),
        "DoubleValue" | "FloatValue" => ty("number"),
        "Int32Value" | "Int64Value" | "UInt32Value" | "UInt64Value" => ty("integer"),
        other => Err(format!("google.protobuf.{other} has no JSON Schema here")),
    })
}
