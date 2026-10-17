//! The ISO schemas and the code generated from them.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::workspace_root;

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

/// DIN SPEC 70121's schemas (the specification's normative annex) as published in the
/// `iso15118` repository of ecog-io. Copyrighted like the others: gitignored, fetched on demand.
const DIN_URL: &str = "https://raw.githubusercontent.com/ecog-io/iso15118/main/iso15118/shared/schemas/";
const DIN_FILES: &[(&str, &str)] = &[
    ("din_spec/V2G_CI_MsgDataTypes.xsd", "V2G_CI_MsgDataTypes.xsd"),
    ("din_spec/V2G_CI_MsgBody.xsd", "V2G_CI_MsgBody.xsd"),
    ("din_spec/V2G_CI_MsgDef.xsd", "V2G_CI_MsgDef.xsd"),
    ("din_spec/V2G_CI_MsgHeader.xsd", "V2G_CI_MsgHeader.xsd"),
    ("xmldsig-core-schema.xsd", "xmldsig-core-schema.xsd"),
];

const ISO2_FILES: &[(&str, &str)] = &[
    ("iso15118_2/V2G_CI_MsgDataTypes.xsd", "V2G_CI_MsgDataTypes.xsd"),
    ("iso15118_2/V2G_CI_MsgBody.xsd", "V2G_CI_MsgBody.xsd"),
    ("iso15118_2/V2G_CI_MsgDef.xsd", "V2G_CI_MsgDef.xsd"),
    ("iso15118_2/V2G_CI_MsgHeader.xsd", "V2G_CI_MsgHeader.xsd"),
    ("xmldsig-core-schema.xsd", "xmldsig-core-schema.xsd"),
];

/// Downloads the XSDs (skipping files already present unless `force`).
pub fn setup(force: bool) -> Result<(), String> {
    let iso20: Vec<(String, &str)> = ISO20_FILES.iter().map(|n| (format!("{ISO20_URL}{n}"), *n)).collect();
    fetch(&workspace_root().join("data/schemas/iso15118-20"), &iso20, force)?;
    let din: Vec<(String, &str)> = DIN_FILES.iter().map(|(from, to)| (format!("{DIN_URL}{from}"), *to)).collect();
    fetch(&workspace_root().join("data/schemas/din-spec"), &din, force)?;
    let iso2: Vec<(String, &str)> = ISO2_FILES.iter().map(|(from, to)| (format!("{DIN_URL}{from}"), *to)).collect();
    fetch(&workspace_root().join("data/schemas/iso15118-2"), &iso2, force)
}

/// Downloads `(url, file name)` into `dir`.
fn fetch(dir: &Path, files: &[(String, &str)], force: bool) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for (url, name) in files {
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
            .arg(url)
            .status()
            .map_err(|e| format!("cannot run curl: {e}"))?;
        if !status.success() {
            return Err(format!("download of {name} failed"));
        }
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Crate directory that holds the generated code of an `exi-gen` group.
pub fn group_dir(group: &exi_gen::Group) -> PathBuf {
    workspace_root().join("crates/codec").join(group.name)
}

pub fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Generates the codecs (gitignored `src/generated` of each codec crate) from the XSDs.
/// Skipped when the output is newer than every schema and than this binary, which carries
/// the generator.
pub fn generate(force: bool) -> Result<(), String> {
    let exe = std::env::current_exe().ok().and_then(|p| mtime(&p));
    for group in exi_gen::GROUPS {
        let schemas = workspace_root().join("data/schemas").join(group.dir);
        let out = group_dir(group).join("src/generated");
        let newest_input = group
            .files
            .iter()
            .filter_map(|f| mtime(&schemas.join(f)))
            .chain(exe)
            .max();
        let up_to_date = mtime(&out.join("mod.rs")).zip(newest_input).is_some_and(|(o, i)| o >= i);
        if up_to_date && !force {
            println!("have  {} codec", group.name);
            continue;
        }
        println!("gen   {} codec", group.name);
        exi_gen::generate(group, &schemas, &out)?;
        let files: Vec<PathBuf> = std::fs::read_dir(&out)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .collect();
        // Readable output is a convenience; a missing rustfmt is not an error.
        let _ = Command::new("rustfmt").args(["--edition", "2024"]).args(&files).status();
    }
    Ok(())
}
