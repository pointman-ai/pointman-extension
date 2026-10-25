//! An extension's own proto, the best practice Pointman's docs/dev/extensions.md describes ("Best practice: your own
//! .proto"): `proto/<id>/v1/<id>.proto`, `package <id>.v1;`, is the source of its manifest's schemas and of its Rust
//! types. The node and core read extension.toml as ever; nothing of theirs changes.
//!
//! - Command `<id>.<verb>` (verb in snake case) takes `<Verb>Params` and answers `<Verb>Result`: `youtube.upload` is
//!   `UploadParams` and `UploadResult`. The settings are `message Settings`.
//! - [`write`] puts each command's `params` and the `[settings]` schema in extension.toml, from those messages, and
//!   keeps the rest of the file as it is (a command's title, lane, outputs, cancellable stay yours). [`check`] fails
//!   while the file says something else: for CI. `pointman-extension manifest write|check` runs them.
//! - [`build`], in the extension's build script, makes buffa's Rust types for the proto, so a handler takes
//!   `UploadParams`, not a `Value` ([`crate::typed`]).
//!
//! What a schema says, field by field: a field's comment is its `description`; buf.validate's rules are the JSON Schema
//! keywords they match (a string's `min_len`, `max_len`, `len`, `pattern`, `in`, `const` and formats; a number's `gt`,
//! `gte`, `lt`, `lte`, `in`, `const`; a list's `min_items`, `max_items`, `unique`, `items`; a map's `min_pairs`,
//! `max_pairs`, `values`; `required`); an enum is a string, one of its values' names in lower case without the enum's
//! prefix (`PRIVACY_PUBLIC` is `public`), and `(pointman.v1.setting).default` is a field's `default`, written as
//! Pointman's own settings write it (a word, a number, true or false, or a list joined by commas). A rule with no
//! keyword is refused rather than left out, so the manifest never says less than the proto.
//!
//! Beside its own files, the proto may import google/protobuf's, `buf/validate/validate.proto` and
//! `pointman/v1/options.proto`: the SDK has them (proto/include).

mod manifest;
mod schema;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use protox::file::{ChainFileResolver, File, FileResolver, GoogleFileResolver, IncludeFileResolver};
use protox::prost::Message as _;
use protox::prost_reflect::prost_types::{DescriptorProto, FileDescriptorSet};
use protox::prost_reflect::{DescriptorPool, MessageDescriptor};

pub use manifest::{check, plan, write, Plan};

const VALIDATE: &str = include_str!("../../proto/include/buf/validate/validate.proto");
const OPTIONS: &str = include_str!("../../proto/include/pointman/v1/options.proto");

/// The files every extension's proto may import without having them: buf.validate's, and Pointman's options
/// (`(pointman.v1.setting)`).
struct Bundled;

impl FileResolver for Bundled {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        match name {
            "buf/validate/validate.proto" => File::from_source(name, VALIDATE),
            "pointman/v1/options.proto" => File::from_source(name, OPTIONS),
            _ => Err(protox::Error::file_not_found(name)),
        }
    }
}

/// An extension's proto, compiled.
pub struct Proto {
    /// The extension's id, from extension.toml.
    pub id: String,
    /// `<id>.v1`.
    pub package: String,
    /// `proto/<id>/v1/<id>.proto`, from the extension's folder.
    pub path: PathBuf,
    pool: DescriptorPool,
    set: FileDescriptorSet,
    /// The extension's own files (`<id>/v1/<id>.proto` and what it imports from proto/), not the bundled ones.
    own: Vec<String>,
}

impl Proto {
    /// The proto of the extension in `dir` (the folder with extension.toml): `proto/<id>/v1/<id>.proto`, with what it
    /// imports from `proto/`, the bundled files, and google/protobuf's.
    pub fn compile(dir: &Path) -> anyhow::Result<Proto> {
        let manifest = crate::load_manifest(&dir.join("extension.toml"))?;
        let id = manifest["extension"]["id"].as_str().context("extension.toml has no [extension] id")?.to_string();
        let name = format!("{id}/v1/{id}.proto");
        let root = dir.join("proto");
        if !root.join(&name).is_file() {
            bail!("no proto/{name} in {}", dir.display());
        }
        let mut resolver = ChainFileResolver::new();
        resolver.add(IncludeFileResolver::new(root));
        resolver.add(Bundled);
        resolver.add(GoogleFileResolver::new());
        let mut compiler = protox::Compiler::with_file_resolver(resolver);
        compiler.include_imports(true).include_source_info(true);
        compiler.open_file(&name).map_err(|e| anyhow::anyhow!("{e:?}")).with_context(|| format!("proto/{name}"))?;
        let set = compiler.file_descriptor_set();
        let own = set
            .file
            .iter()
            .map(|f| f.name().to_string())
            .filter(|n| !n.starts_with("google/protobuf/") && n != "buf/validate/validate.proto")
            .filter(|n| n != "pointman/v1/options.proto")
            .collect();
        Ok(Proto {
            package: format!("{id}.v1"),
            path: PathBuf::from("proto").join(&name),
            id,
            pool: compiler.descriptor_pool(),
            set,
            own,
        })
    }

    /// A message of the extension's package by its name: `UploadParams`.
    pub fn message(&self, name: &str) -> Option<MessageDescriptor> {
        self.pool.get_message_by_name(&format!("{}.{name}", self.package))
    }

    /// The command kinds the proto has params for: `<id>.<verb>` for each `<Verb>Params` of its package.
    pub fn kinds(&self) -> Vec<(String, MessageDescriptor)> {
        self.pool
            .all_messages()
            .filter(|m| m.package_name() == self.package && m.parent_message().is_none())
            .filter_map(|m| {
                let verb = m.name().strip_suffix("Params")?;
                Some((format!("{}.{}", self.id, snake(verb)), m.clone()))
            })
            .collect()
    }

    /// The extension's own files as a FileDescriptorSet's bytes, each field's JSON name its own: what
    /// [`Shapes`](crate::typed::Shapes) reads.
    pub fn descriptors(&self) -> Vec<u8> {
        let mut set = self.with_own_names();
        set.file.retain(|f| self.own.iter().any(|o| o == f.name()));
        set.encode_to_vec()
    }

    /// Every file, the extension's own with each field's JSON name its own (`made_for_kids`, not protobuf's
    /// `madeForKids`), as the manifest's schemas have it.
    fn with_own_names(&self) -> FileDescriptorSet {
        let mut set = self.set.clone();
        for file in set.file.iter_mut().filter(|f| self.own.iter().any(|o| o == f.name())) {
            for m in &mut file.message_type {
                own_json_names(m);
            }
        }
        set
    }

    /// A message's JSON Schema, as the manifest has it (a command's params, the settings).
    pub fn schema(&self, message: &MessageDescriptor) -> anyhow::Result<serde_json::Value> {
        Ok(schema::Schemas::new(&self.pool).message(message)?.json())
    }
}

/// `UploadThumbnail` → `upload_thumbnail`.
fn snake(pascal: &str) -> String {
    let mut out = String::new();
    for (i, c) in pascal.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// `upload_thumbnail` → `UploadThumbnail`: a command's verb as its messages start.
pub fn pascal(verb: &str) -> String {
    verb.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect()
}

/// In an extension's build script (`fn main() { pointman_extension::proto::build().unwrap() }`): buffa's types for its
/// proto, with a [`Typed`](crate::typed::Typed) for each top-level message and a [`Command`](crate::typed::Command) for
/// each command's params whose result is there too, for [`include_proto!`](crate::include_proto). Each field's JSON
/// name is its own (`made_for_kids`), as the manifest's schemas have it. A stale extension.toml is a warning here;
/// `manifest check` is the one that fails.
pub fn build() -> anyhow::Result<()> {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR: run from build.rs")?);
    let out = PathBuf::from(std::env::var("OUT_DIR").context("OUT_DIR: run from build.rs")?);
    println!("cargo:rerun-if-changed={}", dir.join("proto").display());
    println!("cargo:rerun-if-changed={}", dir.join("extension.toml").display());
    let proto = Proto::compile(&dir)?;
    match plan(&dir) {
        Ok(plan) if !plan.stale.is_empty() => {
            println!(
                "cargo:warning=extension.toml is stale ({}): run `pointman-extension manifest write`",
                plan.stale.join(", ")
            )
        }
        Ok(_) => {}
        Err(e) => println!("cargo:warning=extension.toml: {e:#}"),
    }

    std::fs::write(out.join("pointman_proto.pb"), proto.with_own_names().encode_to_vec())?;
    std::fs::write(out.join("pointman_shapes.pb"), proto.descriptors())?;
    buffa_build::Config::new()
        .descriptor_set(out.join("pointman_proto.pb"))
        .files(&proto.own)
        .out_dir(&out)
        .generate_views(false)
        .generate_json(true)
        .idiomatic_enum_aliases(true)
        .include_file("pointman_proto_types.rs")
        .compile()
        .map_err(|e| anyhow::anyhow!("buffa: {e}"))?;
    std::fs::write(out.join("pointman_proto.rs"), glue(&proto, &dir)?)?;
    Ok(())
}

fn own_json_names(m: &mut DescriptorProto) {
    for f in &mut m.field {
        f.json_name = f.name.clone();
    }
    for n in &mut m.nested_type {
        own_json_names(n);
    }
}

/// The module [`include_proto!`](crate::include_proto) brings in: buffa's types, the extension's [`Shapes`](
/// crate::typed::Shapes), and a `Typed` and `Command` for each message.
fn glue(proto: &Proto, dir: &Path) -> anyhow::Result<String> {
    let manifest = crate::load_manifest(&dir.join("extension.toml"))?;
    let kinds: BTreeSet<&str> =
        manifest["commands"].as_array().into_iter().flatten().filter_map(|c| c["kind"].as_str()).collect();
    let rust = |m: &MessageDescriptor| format!("{}::{}", m.package_name().replace('.', "::"), m.name());
    let mut out = format!(
        "// Made by pointman_extension::proto::build from {}: buffa's types, and what the SDK needs to read and write\n\
         // them as the manifest's JSON.\n\n\
         include!(concat!(env!(\"OUT_DIR\"), \"/pointman_proto_types.rs\"));\n\n\
         /// What the extension's descriptors say about its messages' JSON.\n\
         pub fn shapes() -> &'static ::pointman_extension::typed::Shapes {{\n    \
         static SHAPES: ::std::sync::OnceLock<::pointman_extension::typed::Shapes> = ::std::sync::OnceLock::new();\n    \
         SHAPES.get_or_init(|| {{\n        \
         ::pointman_extension::typed::Shapes::decode(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/pointman_shapes.pb\")))\n            \
         .expect(\"the descriptors build.rs wrote\")\n    \
         }})\n}}\n",
        proto.path.display()
    );
    let tops = proto
        .pool
        .all_messages()
        .filter(|m| m.parent_message().is_none() && proto.own.iter().any(|o| o == m.parent_file().name()));
    for m in tops {
        write!(
            out,
            "\nimpl ::pointman_extension::typed::Typed for {} {{\n    const NAME: &'static str = {:?};\n    \
             fn shapes() -> &'static ::pointman_extension::typed::Shapes {{\n        shapes()\n    }}\n}}\n",
            rust(&m),
            m.full_name()
        )?;
    }
    for (kind, params) in proto.kinds() {
        let verb = params.name().trim_end_matches("Params");
        let Some(result) = proto.message(&format!("{verb}Result")) else { continue };
        if !kinds.contains(kind.as_str()) {
            continue; // manifest::plan says so
        }
        write!(
            out,
            "\nimpl ::pointman_extension::typed::Command for {} {{\n    const KIND: &'static str = {kind:?};\n    \
             type Result = {};\n}}\n",
            rust(&params),
            rust(&result)
        )?;
    }
    Ok(out)
}
