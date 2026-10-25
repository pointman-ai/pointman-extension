//! extension.toml's schemas from the proto: each command's `params` and the `[settings]`, the rest of the file kept as
//! it is.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

use super::schema::{Node, Schemas};
use super::{pascal, Proto};

/// What [`write`] would change in an extension's manifest.
#[derive(Debug)]
pub struct Plan {
    /// extension.toml.
    pub path: PathBuf,
    /// The file as it would be.
    pub text: String,
    /// What says something else than the proto now: `youtube.upload's params`, `[settings]`.
    pub stale: Vec<String>,
    /// Commands the proto has no params for, whose params stay as written.
    pub kept: Vec<String>,
}

/// What extension.toml would be with its schemas from the proto, for the extension in `dir`.
pub fn plan(dir: &Path) -> anyhow::Result<Plan> {
    let proto = Proto::compile(dir)?;
    let path = dir.join("extension.toml");
    let before = std::fs::read_to_string(&path).with_context(|| format!("can't read {}", path.display()))?;
    let now = crate::load_manifest(&path)?;
    let mut doc: DocumentMut = before.parse().with_context(|| format!("{} isn't valid TOML", path.display()))?;
    let mut schemas = Schemas::new(&proto.pool);
    let source = proto.path.display().to_string();
    let (mut stale, mut kept) = (Vec::new(), Vec::new());

    let kinds: Vec<String> =
        now["commands"].as_array().into_iter().flatten().filter_map(|c| c["kind"].as_str().map(String::from)).collect();
    for (kind, params) in proto.kinds() {
        if !kinds.contains(&kind) {
            bail!(
                "{source} has {} but extension.toml has no [[commands]] with kind = {kind:?}: add it, with its title",
                params.name()
            );
        }
    }
    if let Some(commands) = doc.get_mut("commands").and_then(Item::as_array_of_tables_mut) {
        for (i, command) in commands.iter_mut().enumerate() {
            let Some(kind) = command.get("kind").and_then(Item::as_str).map(String::from) else { continue };
            let verb = kind.strip_prefix(&format!("{}.", proto.id)).filter(|v| {
                v.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            });
            let name = verb.map(|v| format!("{}Params", pascal(v)));
            let Some(message) = name.as_deref().and_then(|n| proto.message(n)) else {
                kept.push(kind);
                continue;
            };
            let schema = schemas.message(&message)?;
            if now["commands"][i]["params"] != schema.json() {
                stale.push(format!("{kind}'s params"));
            }
            command.remove("params");
            let mut table = table(&schema);
            table
                .decor_mut()
                .set_prefix(format!("\n# From {} in {source}: `pointman-extension manifest write`\n", message.name()));
            command.insert("params", Item::Table(table));
        }
    }
    if let Some(message) = proto.message("Settings") {
        let schema = schemas.message(&message)?;
        if now["settings"] != schema.json() {
            stale.push("[settings]".into());
        }
        let position = doc.get("settings").and_then(Item::as_table).and_then(Table::position);
        let mut table = table(&schema);
        table.set_position(position);
        table.decor_mut().set_prefix(format!("\n# From Settings in {source}: `pointman-extension manifest write`\n"));
        doc.insert("settings", Item::Table(table));
    }
    let mut text = doc.to_string();
    if stale.is_empty() {
        text = before; // only how it's written differs: nothing to write
    }
    Ok(Plan { path, text, stale, kept })
}

/// Writes the schemas from the proto into extension.toml, for the extension in `dir`: what changed (none when it was
/// current).
pub fn write(dir: &Path) -> anyhow::Result<Plan> {
    let plan = plan(dir)?;
    if !plan.stale.is_empty() {
        std::fs::write(&plan.path, &plan.text).with_context(|| format!("can't write {}", plan.path.display()))?;
    }
    Ok(plan)
}

/// Fails while extension.toml's schemas say something else than the proto, naming what: for CI.
pub fn check(dir: &Path) -> anyhow::Result<Plan> {
    let plan = plan(dir)?;
    if !plan.stale.is_empty() {
        bail!(
            "{} is stale: {} differ from the proto. Run `pointman-extension manifest write`",
            plan.path.display(),
            plan.stale.join(", ")
        );
    }
    Ok(plan)
}

/// A schema as TOML: a table for it, for its properties (without a header of their own) and for each property, and an
/// inline table for the rest (a list's items, `not`), unless it has properties of its own.
fn table(schema: &Node) -> Table {
    let mut table = Table::new();
    let Node::Obj(obj) = schema else { return table };
    for (key, value) in obj {
        match (key.as_str(), value) {
            ("properties", Node::Obj(props)) => {
                let mut map = Table::new();
                map.set_implicit(true);
                for (name, prop) in props {
                    map.insert(name, Item::Table(self::table(prop)));
                }
                table.insert(key, Item::Table(map));
            }
            (_, Node::Obj(inner)) if inner.iter().any(|(k, _)| k == "properties") => {
                table.insert(key, Item::Table(self::table(value)));
            }
            _ => {
                table.insert(key, Item::Value(toml(value)));
            }
        }
    }
    table
}

fn toml(node: &Node) -> toml_edit::Value {
    match node {
        Node::Str(s) => s.as_str().into(),
        Node::Int(i) => (*i).into(),
        Node::Float(f) => (*f).into(),
        Node::Bool(b) => (*b).into(),
        Node::List(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(toml(item));
            }
            array.into()
        }
        Node::Obj(obj) => {
            let mut inline = InlineTable::new();
            for (k, v) in obj {
                inline.insert(k, toml(v));
            }
            inline.into()
        }
    }
}
