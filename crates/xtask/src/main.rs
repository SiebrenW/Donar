//! Project automation. Run as `cargo xtask <command>`.

mod container;
mod java;
mod schemas;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("setup") => {
            let force = args.any(|a| a == "--force");
            schemas::setup(force).and_then(|()| schemas::generate(force))
        }
        Some("gen") => schemas::generate(args.any(|a| a == "--force")),
        Some("vectors") => java::vectors(),
        Some("grammars") => java::grammars(),
        Some("linux-test") => container::linux_test(args.collect()),
        Some("interop") => container::interop(args.collect()),
        _ => {
            eprintln!("usage: cargo xtask setup [--force] | gen [--force] | vectors | grammars | linux-test [cargo test args] | interop [partner] [evcc args]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The repository root. Taken from the environment cargo sets when it runs us, not from the
/// path baked in at compile time: a build output can be reused after the repository moved
/// (a different mount point in a container).
fn workspace_root() -> PathBuf {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")), PathBuf::from);
    manifest.parent().and_then(Path::parent).unwrap().to_path_buf()
}

fn run(cmd: &mut Command) -> Result<(), String> {
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{cmd:?} exited with {s}")),
        Err(e) => Err(format!("cannot run {cmd:?}: {e}")),
    }
}
