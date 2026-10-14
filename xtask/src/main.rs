//! Project automation. Run as `cargo xtask <command>`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// ISO publishes the -20 schemas here. They are free to download but
/// copyrighted, so they are gitignored and fetched on demand.
const ISO20_URL: &str = "https://standards.iso.org/iso/15118/-20/ed-1/en/";
const ISO20_FILES: &[&str] = &[
    "V2G_CI_AppProtocol.xsd",
    "V2G_CI_CommonTypes.xsd",
    "V2G_CI_CommonMessages.xsd",
    "V2G_CI_DC.xsd",
    "V2G_CI_AC.xsd",
    "V2G_CI_ACDP.xsd",
    "V2G_CI_WPT.xsd",
    "xmldsig-core-schema.xsd",
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("setup") => setup(args.any(|a| a == "--force")),
        _ => {
            eprintln!("usage: cargo xtask setup [--force]");
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

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Downloads the XSDs (skipping files already present unless `force`).
/// Code generation is hooked in here once `exi-gen` exists.
fn setup(force: bool) -> Result<(), String> {
    let dir = workspace_root().join("schemas/iso15118-20");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for name in ISO20_FILES {
        let dest = dir.join(name);
        if dest.exists() && !force {
            println!("have  {name}");
            continue;
        }
        println!("fetch {name}");
        let tmp = dest.with_extension("part");
        let status = Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location", "--output"])
            .arg(&tmp)
            .arg(format!("{ISO20_URL}{name}"))
            .status()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        if !status.success() {
            return Err(format!("download of {name} failed"));
        }
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    Ok(())
}
