//! `pointman-extension manifest write|check [folder]`: an extension's manifest schemas from its own proto
//! (`pointman_extension::proto`). `write` puts each command's params and the `[settings]` schema in extension.toml from
//! proto/<id>/v1/<id>.proto, keeping the rest of the file; `check` fails while the file is stale, for CI.

use std::path::PathBuf;
use std::process::ExitCode;

use pointman_extension::proto;

const USAGE: &str = "usage: pointman-extension manifest write|check [folder]
  write   puts each command's params and the [settings] schema in extension.toml, from proto/<id>/v1/<id>.proto
  check   fails while extension.toml says something else than the proto (for CI)
  folder  the extension's, with extension.toml (the working folder if not given)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (verb, dir) = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["manifest", verb] => (verb.to_string(), PathBuf::from(".")),
        ["manifest", verb, dir] => (verb.to_string(), PathBuf::from(dir)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match verb.as_str() {
        "write" => proto::write(&dir).inspect(|plan| {
            if plan.stale.is_empty() {
                println!("{} is up to date", plan.path.display());
            } else {
                println!("wrote {}: {}", plan.path.display(), plan.stale.join(", "));
            }
        }),
        "check" => proto::check(&dir).inspect(|plan| println!("{} matches the proto", plan.path.display())),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(plan) => {
            for kind in &plan.kept {
                println!("{kind}: no params message in the proto, so its params stay as written");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}
