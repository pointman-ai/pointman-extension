//! Writes `proto/descriptors.pb`: the descriptors this crate's types are built from.
//!
//! Where they come from: Pointman's core repo (pointman-ai/pointman, private) keeps `src/pointman/descriptors.pb`,
//! the compiled `proto/` tree (`proto/tools/gen.sh`). The extension protocol is
//! `proto/pointman/extension/v1/protocol.proto` there, with PaneContent, PaneEdit and PaneInput in
//! `proto/pointman/v1/panes.proto`. This keeps protocol.proto and the files it imports, all the way down (the
//! closure), and drops their comments (source info), so a build needs neither protoc nor core's checkout.
//!
//! Run it from a checkout of core, on the branch whose protocol this crate follows, when the protocol changes:
//!
//! ```text
//! cargo run --example trim_descriptors -- <core>/src/pointman/descriptors.pb proto/descriptors.pb
//! ```
//!
//! The set's rev is written beside it as `proto/SOURCE`: pass it as a third argument (core's commit, say).

use std::collections::{BTreeSet, HashMap};

use buffa::Message;
use buffa_descriptor::generated::descriptor::FileDescriptorSet;

const ROOT: &str = "pointman/extension/v1/protocol.proto";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to] =
        [args.first(), args.get(1)].map(|a| a.expect("usage: trim_descriptors <core descriptors.pb> <out>"));
    let bytes = std::fs::read(from).unwrap_or_else(|e| panic!("can't read {from}: {e}"));
    let all = FileDescriptorSet::decode_from_slice(&bytes).expect("a FileDescriptorSet");
    let by_name: HashMap<&str, usize> =
        all.file.iter().enumerate().filter_map(|(i, f)| Some((f.name.as_deref()?, i))).collect();

    let mut keep: BTreeSet<usize> = BTreeSet::new();
    let mut todo = vec![ROOT.to_string()];
    while let Some(name) = todo.pop() {
        let i = *by_name.get(name.as_str()).unwrap_or_else(|| panic!("{name} isn't in {from}"));
        if keep.insert(i) {
            todo.extend(all.file[i].dependency.iter().cloned());
        }
    }
    // the set's own order is already dependencies first
    let mut out = FileDescriptorSet::default();
    for i in keep {
        let mut file = all.file[i].clone();
        file.source_code_info = Default::default();
        out.file.push(file);
    }
    let names: Vec<&str> = out.file.iter().filter_map(|f| f.name.as_deref()).collect();
    std::fs::write(to, out.encode_to_vec()).unwrap_or_else(|e| panic!("can't write {to}: {e}"));
    eprintln!("{} files, from {} in {from}:\n  {}", names.len(), all.file.len(), names.join("\n  "));
    if let Some(rev) = args.get(2) {
        let note = format!("core: pointman-ai/pointman {rev}\nfrom: src/pointman/descriptors.pb, closure of {ROOT}\n");
        std::fs::write(std::path::Path::new(to).with_file_name("SOURCE"), note).expect("proto/SOURCE");
    }
}
