//! An extension's own proto made into its manifest's schemas (`pointman_extension::proto`): every rule the generator
//! knows, on tests/fixtures/proto/kitchen, and `write` and `check` on a copy of it.
#![cfg(feature = "proto")]

use std::path::{Path, PathBuf};

use pointman_extension::proto::{self, Proto};
use pointman_extension::typed::Shapes;
use serde_json::{json, Value};

const KITCHEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/proto/kitchen");

/// A copy of the kitchen extension to change.
fn kitchen() -> (tempdir::Dir, PathBuf) {
    let dir = tempdir::Dir::new();
    copy(Path::new(KITCHEN), dir.path());
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!("pointman-manifest-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Dir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn schema(proto: &Proto, message: &str) -> Value {
    proto.schema(&proto.message(message).unwrap()).unwrap()
}

fn manifest(dir: &Path) -> Value {
    let text = std::fs::read_to_string(dir.join("extension.toml")).unwrap();
    serde_json::to_value(toml::from_str::<toml::Table>(&text).unwrap()).unwrap()
}

#[test]
fn every_rule_says_its_json_schema_keyword() {
    let proto = Proto::compile(Path::new(KITCHEN)).unwrap();
    let privacy = json!(["private", "unlisted", "public"]);
    let expected = json!({
        "type": "object",
        "description": "Uploads a video to the channel.",
        "properties": {
            // a comment above is its description; required is the schema's required
            "file": {"type": "string", "description": "The video file: a board file or a path in the extension's folders."},
            "title": {"type": "string", "description": "The video's title, as viewers see it.", "minLength": 1, "maxLength": 100},
            // a comment after it on its line, when there's none above
            "description": {"type": "string", "description": "Below the video.", "maxLength": 5000},
            "tags": {"type": "array", "description": "Words people search by.",
                     "items": {"type": "string", "maxLength": 30}, "maxItems": 30, "uniqueItems": true},
            // an enum: its values' names in lower case without its prefix, its UNSPECIFIED left out
            "privacy": {"type": "string", "description": "Who can watch it.", "enum": privacy, "default": "private"},
            "publish_at": {"type": "string", "description": "When it goes public: RFC 3339.", "format": "date-time"},
            "made_for_kids": {"type": "boolean", "description": "For kids, as YouTube's law says.", "default": false},
            "language": {"type": "string", "pattern": "^[a-z]{2}$", "minLength": 2, "maxLength": 2},
            "category": {"type": "integer", "description": "Its category's number.", "minimum": 1, "maximum": 44},
            "size": {"type": "integer", "exclusiveMinimum": 0, "exclusiveMaximum": 137438953472i64},
            "volume": {"type": "number", "default": 0.5, "minimum": 0.0, "maximum": 1.0},
            "speed": {"type": "number", "exclusiveMinimum": 0.0, "maximum": 4.0},
            "copies": {"type": "integer", "enum": [1, 2, 4]},
            "region": {"type": "string", "enum": ["eu", "us"]},
            "slug": {"type": "string", "not": {"enum": ["admin"]}},
            "contact": {"type": "string", "format": "email"},
            "link": {"type": "string", "format": "uri"},
            // a message in a message: its own schema, its own required
            "place": {"type": "object", "description": "Where it was shot.", "properties": {
                "name": {"type": "string"},
                "lat": {"type": "number", "minimum": -90.0, "maximum": 90.0},
                // an enum field with no comment of its own takes its enum's
                "shown": {"type": "string", "description": "Who can watch a video.", "enum": privacy},
            }, "required": ["name"], "additionalProperties": false},
            "chapters": {"type": "array", "items": {"type": "object", "properties": {
                "at": {"type": "integer", "description": "From the start, in seconds."},
                "title": {"type": "string", "minLength": 1},
            }, "additionalProperties": false}, "minItems": 1},
            "labels": {"type": "object", "additionalProperties": {"type": "string", "maxLength": 20}, "maxProperties": 8},
            "length": {"type": "string", "pattern": "^-?[0-9]+(\\.[0-9]{1,9})?s$"},
            "extra": {"type": "object"},
            "cover": {"type": "string", "contentEncoding": "base64"},
            "fallback": {"type": "string", "description": "Who can watch a video.", "enum": ["private", "unlisted"]},
            "allowed": {"type": "array",
                        "items": {"type": "string", "description": "Who can watch a video.", "enum": privacy},
                        "default": ["public", "unlisted"]},
            "audience": {"type": "string", "enum": ["yes", "no"]},
            "playlist": {"type": "string"},
            "drafts": {"type": "boolean"},
            "by_region": {"type": "object",
                          "additionalProperties": {"type": "string", "description": "Who can watch a video.", "enum": privacy}},
            "sizes": {"type": "array", "items": {"type": "integer"}},
            // google.protobuf.Value: any JSON, an empty schema
            "anything": {"description": "Anything at all."},
        },
        "required": ["file"],
        // a message is closed: inputs are strict
        "additionalProperties": false,
        // a oneof buf.validate requires: exactly one of its fields
        "oneOf": [{"required": ["playlist"]}, {"required": ["drafts"]}],
    });
    let got = schema(&proto, "UploadParams");
    for (name, prop) in expected["properties"].as_object().unwrap() {
        assert_eq!(&got["properties"][name], prop, "{name}");
    }
    assert_eq!(got, expected);
}

#[test]
fn settings_and_their_defaults() {
    let proto = Proto::compile(Path::new(KITCHEN)).unwrap();
    assert_eq!(
        schema(&proto, "Settings"),
        json!({
            "type": "object",
            "description": "This machine's settings for the extension.",
            "properties": {
                "channel": {"type": "string", "description": "The channel to upload to, by its handle.",
                            "pattern": "^@[A-Za-z0-9._-]{3,30}$"},
                "privacy": {"type": "string", "description": "Who can watch a new upload until someone says otherwise.",
                            "enum": ["private", "unlisted", "public"], "default": "private"},
                "uploads": {"type": "integer", "description": "How many uploads may run at once.", "default": 1,
                            "minimum": 1, "maximum": 4},
                "notify": {"type": "boolean", "description": "Whether to notify subscribers.", "default": true},
            },
            "additionalProperties": false,
        })
    );
}

#[test]
fn write_puts_the_schemas_in_and_keeps_the_rest() {
    let (_dir, dir) = kitchen();
    let before = manifest(&dir);
    let plan = proto::write(&dir).unwrap();
    assert_eq!(
        plan.stale,
        ["kitchen.upload's params", "kitchen.set_thumbnail's params", "kitchen.ping's params", "[settings]"]
    );
    assert_eq!(plan.kept, ["kitchen.status"]);

    let proto = Proto::compile(&dir).unwrap();
    let after = manifest(&dir);
    assert_eq!(after["commands"][0]["params"], schema(&proto, "UploadParams"));
    assert_eq!(after["commands"][2]["params"], schema(&proto, "SetThumbnailParams"));
    assert_eq!(after["settings"], schema(&proto, "Settings"));
    // a message with no fields: no properties at all, so TOML keeps all it says
    assert_eq!(
        after["commands"][3]["params"],
        json!({"type": "object", "description": "Takes nothing.", "additionalProperties": false})
    );
    // what's hand-written stays: the rest of each command, the command with no params message, the other tables
    for key in ["kind", "title", "lane", "cancellable", "outputs"] {
        assert_eq!(after["commands"][0][key], before["commands"][0][key], "{key}");
    }
    assert_eq!(after["commands"][1], before["commands"][1]);
    for key in ["extension", "run", "events"] {
        assert_eq!(after[key], before[key], "{key}");
    }
    let text = std::fs::read_to_string(dir.join("extension.toml")).unwrap();
    assert!(text.starts_with("# A test extension whose proto says every rule"), "{text}");
    assert!(text.contains("title = \"Upload a video\"   # stays as written"), "{text}");
    assert!(text.contains("# no StatusParams: kept"), "{text}");
    assert!(text.contains("# From UploadParams in proto/kitchen/v1/kitchen.proto"), "{text}");
    assert!(text.contains("[commands.params.properties.title]\ntype = \"string\""), "{text}");

    // once written, it's current: writing again changes nothing, and check passes
    assert!(proto::write(&dir).unwrap().stale.is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("extension.toml")).unwrap(), text);
    proto::check(&dir).unwrap();
}

#[test]
fn check_fails_while_the_file_is_stale() {
    let (_dir, dir) = kitchen();
    // never written
    let e = proto::check(&dir).unwrap_err().to_string();
    assert!(
        e.contains(
            "is stale: kitchen.upload's params, kitchen.set_thumbnail's params, kitchen.ping's params, [settings]"
        ),
        "{e}"
    );
    assert!(e.contains("pointman-extension manifest write"), "{e}");

    // written, then the proto changes: a field more
    proto::write(&dir).unwrap();
    let file = dir.join("proto/kitchen/v1/kitchen.proto");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("message SetThumbnailResult {}", "message SetThumbnailResult {}\n")).unwrap();
    proto::check(&dir).unwrap(); // only how it's written: still current
    let changed = text.replace("string file = 2 [(buf.validate.field).required = true];\n}", "string file = 2;\n}");
    assert_ne!(changed, text);
    std::fs::write(&file, &changed).unwrap();
    let e = proto::check(&dir).unwrap_err().to_string();
    assert!(e.contains("is stale: kitchen.set_thumbnail's params differ"), "{e}");

    // or the file is edited by hand
    proto::write(&dir).unwrap();
    let toml = dir.join("extension.toml");
    let text = std::fs::read_to_string(&toml).unwrap();
    std::fs::write(&toml, text.replace("maxLength = 5000", "maxLength = 500")).unwrap();
    let e = proto::check(&dir).unwrap_err().to_string();
    assert!(e.contains("is stale: kitchen.upload's params differ"), "{e}");
}

#[test]
fn a_params_message_needs_its_command() {
    let (_dir, dir) = kitchen();
    let file = dir.join("proto/kitchen/v1/kitchen.proto");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("{text}\nmessage DeleteParams {{}}\n")).unwrap();
    let e = proto::check(&dir).unwrap_err().to_string();
    assert!(
        e.contains("has DeleteParams but extension.toml has no [[commands]] with kind = \"kitchen.delete\""),
        "{e}"
    );
}

#[test]
fn a_rule_with_no_keyword_is_refused() {
    let (_dir, dir) = kitchen();
    let file = dir.join("proto/kitchen/v1/kitchen.proto");
    let text = std::fs::read_to_string(&file).unwrap();
    let changed = text.replace("string.max_len = 5000]", "string.prefix = \"A\"]");
    assert_ne!(changed, text);
    std::fs::write(&file, changed).unwrap();
    let e = format!("{:#}", proto::check(&dir).unwrap_err());
    assert!(
        e.contains(
            "kitchen.v1.UploadParams.description: (buf.validate.field).string.prefix has no JSON Schema keyword"
        ),
        "{e}"
    );
}

#[test]
fn a_byte_count_says_to_count_characters() {
    let (_dir, dir) = kitchen();
    let file = dir.join("proto/kitchen/v1/kitchen.proto");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("string.max_len = 5000]", "string.max_bytes = 5000]")).unwrap();
    let e = format!("{:#}", proto::check(&dir).unwrap_err());
    assert!(
        e.contains(
            "kitchen.v1.UploadParams.description: (buf.validate.field).string.max_bytes has no JSON Schema keyword"
        ),
        "{e}"
    );
    assert!(e.contains("so say it with max_len and check the bytes in code"), "{e}");
}

#[test]
fn a_default_must_fit_its_field() {
    let (_dir, dir) = kitchen();
    let file = dir.join("proto/kitchen/v1/kitchen.proto");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("default = \"private\"];\n  // When", "default = \"secret\"];\n  // When"))
        .unwrap();
    let e = format!("{:#}", proto::check(&dir).unwrap_err());
    assert!(
        e.contains("kitchen.v1.UploadParams.privacy: its default \"secret\" isn't one of private, unlisted, public"),
        "{e}"
    );
}

#[test]
fn no_proto_says_where_it_looked() {
    let (_dir, dir) = kitchen();
    std::fs::remove_dir_all(dir.join("proto")).unwrap();
    let e = proto::check(&dir).unwrap_err().to_string();
    assert!(e.contains("no proto/kitchen/v1/kitchen.proto in"), "{e}");
}

/// The JSON a typed handler reads and writes is the manifest's: enums by their words, 64-bit integers as numbers.
#[test]
fn shapes_go_between_the_manifests_json_and_protobufs() {
    let proto = Proto::compile(Path::new(KITCHEN)).unwrap();
    let shapes = Shapes::decode(&proto.descriptors()).unwrap();

    let mut params = json!({
        "file": "a.mp4", "privacy": "public", "allowed": ["private", "unlisted"],
        "place": {"name": "here", "shown": "unlisted"}, "by_region": {"eu": "public"}, "size": 12,
    });
    shapes.to_proto("kitchen.v1.UploadParams", &mut params).unwrap();
    assert_eq!(
        params,
        json!({
            "file": "a.mp4", "privacy": "PRIVACY_PUBLIC", "allowed": ["PRIVACY_PRIVATE", "PRIVACY_UNLISTED"],
            "place": {"name": "here", "shown": "PRIVACY_UNLISTED"}, "by_region": {"eu": "PRIVACY_PUBLIC"}, "size": 12,
        })
    );

    let mut wrong = json!({"place": {"name": "here", "nmae": "there"}});
    let e = shapes.to_proto("kitchen.v1.UploadParams", &mut wrong).unwrap_err().to_string();
    assert_eq!(e, "place.nmae isn't a field: it takes name, lat, shown");

    let mut wrong = json!({"place": {"shown": "secret"}});
    let e = shapes.to_proto("kitchen.v1.UploadParams", &mut wrong).unwrap_err().to_string();
    assert_eq!(e, "place.shown: \"secret\" isn't one of private, unlisted, public");

    let mut result = json!({
        "video": "v1", "privacy": "PRIVACY_UNLISTED", "views": "9007199254740993", "also": ["PRIVACY_PUBLIC"],
        "place": {"shown": "PRIVACY_PRIVATE"}, "by_region": {"us": "PRIVACY_PRIVATE"},
    });
    shapes.from_proto("kitchen.v1.UploadResult", &mut result);
    assert_eq!(
        result,
        json!({
            "video": "v1", "privacy": "unlisted", "views": 9007199254740993u64, "also": ["public"],
            "place": {"shown": "private"}, "by_region": {"us": "private"},
        })
    );
}
