//! The feature combinations of the cores that have to build: `cargo xtask features [--target TRIPLE]`.
//!
//! The features are additive, so the workspace builds everything at once; a product builds a few of them with `strict`, which rejects
//! what cannot exist (AC with a pantograph, a plug with a pantograph, a protocol without energy). A hand-picked list, not the power
//! set: each row is something somebody builds. A new feature of the cores has to appear in one of them (a check below).

use std::process::Command;

use crate::{run, workspace_root};

const CORES: [&str; 2] = ["evcc-core", "evse-core"];

/// `(what it is, features)`; `None` is the default feature set.
const SETS: [(&str, Option<&str>); 10] = [
    ("the default", None),
    ("DIN SPEC 70121, DC", Some("strict,din")),
    ("ISO 15118-2, AC only", Some("strict,iso2,ac")),
    ("ISO 15118-2, DC only", Some("strict,iso2,dc")),
    ("ISO 15118-20, AC only", Some("strict,iso20,plug,ac")),
    ("ISO 15118-20, DC only", Some("strict,iso20,plug,dc")),
    ("ACDP (the inverted pantograph)", Some("strict,acdp")),
    ("everything (a plug and a pantograph in one build: the simulators)", Some("iso20,iso2,din,slac,ac,dc,acdp")),
    ("no AC anywhere", Some("strict,iso20,iso2,din,slac,dc")),
    ("no DC anywhere", Some("strict,iso20,iso2,slac,ac")),
];

pub fn check(args: Vec<String>) -> Result<(), String> {
    let target = match args.as_slice() {
        [] => None,
        [flag, triple] if flag == "--target" => Some(triple.clone()),
        _ => return Err("usage: cargo xtask features [--target TRIPLE]".into()),
    };
    every_feature_is_covered()?;
    for (what, features) in SETS {
        eprintln!("== {what}: {}", features.unwrap_or("default features"));
        // Warnings are errors: a build with a feature off must not leave unused code behind.
        let mut cmd = Command::new("cargo");
        cmd.env("RUSTFLAGS", "-D warnings").args(["check", "--locked"]);
        for core in CORES {
            cmd.args(["-p", core]);
        }
        if let Some(features) = features {
            cmd.args(["--no-default-features", "--features", features]);
        }
        if let Some(target) = &target {
            cmd.args(["--target", target]);
        }
        run(&mut cmd)?;
    }
    Ok(())
}

/// Every feature the cores declare is built by one of the sets, directly or because a feature of a set implies it (not `default`
/// and `strict`, which are no connectors or protocols).
fn every_feature_is_covered() -> Result<(), String> {
    for core in ["crates/evcc/core", "crates/evse/core"] {
        let manifest = std::fs::read_to_string(workspace_root().join(core).join("Cargo.toml")).map_err(|e| e.to_string())?;
        let section = manifest.split("[features]").nth(1).and_then(|s| s.split("\n[").next()).ok_or("no [features]")?;
        // name -> the features it implies (`dep:` entries are no features)
        let mut declared: Vec<(&str, Vec<&str>)> = Vec::new();
        for line in section.lines() {
            let Some((name, value)) = line.split_once(" = ") else { continue };
            let name = name.trim();
            if name.starts_with('#') {
                continue;
            }
            let implied = value.split('"').skip(1).step_by(2).filter(|f| !f.starts_with("dep:")).collect();
            declared.push((name, implied));
        }
        let mut covered: Vec<&str> = SETS.iter().filter_map(|(_, f)| *f).flat_map(|f| f.split(',')).collect();
        loop {
            let before = covered.len();
            for (name, implied) in &declared {
                if covered.contains(name) {
                    for f in implied {
                        if !covered.contains(f) {
                            covered.push(f);
                        }
                    }
                }
            }
            if covered.len() == before {
                break;
            }
        }
        for (name, _) in &declared {
            if *name != "default" && !covered.contains(name) {
                return Err(format!("the feature `{name}` of {core} is built by none of the sets of crates/xtask/src/features.rs"));
            }
        }
    }
    Ok(())
}
