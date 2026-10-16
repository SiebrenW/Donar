//! The independent EXI oracle: EXIficient, driven through small Java programs.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{run, workspace_root};

const EXIFICIENT: &[&str] = &[
    "com/siemens/ct/exi/exificient/1.0.7/exificient-1.0.7.jar",
    "com/siemens/ct/exi/exificient-core/1.0.7/exificient-core-1.0.7.jar",
    "com/siemens/ct/exi/exificient-grammars/1.0.7/exificient-grammars-1.0.7.jar",
    "org/slf4j/slf4j-api/2.0.6/slf4j-api-2.0.6.jar",
    "xmlpull/xmlpull/1.1.3.1/xmlpull-1.1.3.1.jar",
    "xerces/xercesImpl/2.12.2/xercesImpl-2.12.2.jar",
    "xml-apis/xml-apis/1.4.01/xml-apis-1.4.01.jar",
];

/// `java`/`javac` from PATH, else Homebrew's keg-only openjdk.
fn jdk_tool(name: &str) -> PathBuf {
    let brew = Path::new("/opt/homebrew/opt/openjdk/bin").join(name);
    let on_path = Command::new(name).arg("-version").output().is_ok_and(|o| o.status.success());
    if on_path || !brew.exists() { PathBuf::from(name) } else { brew }
}

/// Downloads EXIficient and compiles the helper programs in `crates/xtask/java`; returns the class
/// directory and the class path.
fn java_env() -> Result<(PathBuf, String), String> {
    let root = workspace_root();
    let lib = root.join("data/vectors/lib");
    std::fs::create_dir_all(&lib).map_err(|e| e.to_string())?;
    for path in EXIFICIENT {
        let dest = lib.join(path.rsplit('/').next().unwrap());
        if dest.exists() {
            continue;
        }
        println!("fetch {}", dest.file_name().unwrap().to_string_lossy());
        run(Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location", "--output"])
            .arg(&dest)
            .arg(format!("https://repo1.maven.org/maven2/{path}")))?;
    }
    let classes = root.join("data/vectors/classes");
    std::fs::create_dir_all(&classes).map_err(|e| e.to_string())?;
    let cp = format!("{}/*", lib.display());
    let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join("crates/xtask/java"))
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "java"))
        .collect();
    sources.sort();
    run(Command::new(jdk_tool("javac")).args(["-cp", &cp, "-d"]).arg(&classes).args(&sources))?;
    Ok((classes.clone(), format!("{}:{cp}", classes.display())))
}

/// A copy of the schemas for the Java tools. `xmldsig-core-schema.xsd` has a DOCTYPE naming
/// the XMLSchema DTD at w3.org, which Xerces then downloads on every run and which
/// rate-limits; nothing in the schema uses what the DTD declares, so the copy omits it.
fn java_schemas() -> Result<PathBuf, String> {
    let root = workspace_root();
    let (from, to) = (root.join("data/schemas/iso15118-20"), root.join("data/vectors/schemas"));
    std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&from).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|x| x != "xsd") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let text = match (text.find("<!DOCTYPE"), text.find("]>")) {
            (Some(a), Some(b)) if a < b => format!("{}{}", &text[..a], &text[b + 2..]),
            _ => text,
        };
        std::fs::write(to.join(path.file_name().unwrap()), text).map_err(|e| e.to_string())?;
    }
    Ok(to)
}

/// Runs a helper program, retrying a few times in case of transient failures.
fn java_run(cp: &str, class: &str, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    let mut attempt = 0;
    loop {
        let out = Command::new(jdk_tool("java"))
            .args(["-cp", cp, class])
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        attempt += 1;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        if attempt == 5 {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Encodes every XML in `crates/xtask/vectors` with EXIficient into `data/vectors/<name>.hex`.
/// These are the independent oracle for the codecs; they are gitignored.
pub fn vectors() -> Result<(), String> {
    let root = workspace_root();
    let (_, cp) = java_env()?;
    let java_schemas = java_schemas()?;
    let manifest = std::fs::read_to_string(root.join("crates/xtask/vectors/manifest.txt"))
        .map_err(|e| e.to_string())?;
    for line in manifest.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let mut it = line.split_whitespace();
        let (name, xsd) = (it.next().unwrap(), it.next().ok_or("manifest: missing xsd")?);
        // An optional third column, `fragment`, encodes the document as a fragment (signed parts).
        let fragment = it.next() == Some("fragment");
        let xml = root.join(format!("crates/xtask/vectors/{name}.xml"));
        let dest = root.join(format!("data/vectors/{name}.hex"));
        let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        if mtime(&dest).is_some_and(|h| mtime(&xml).is_some_and(|x| h >= x)) {
            continue; // up to date; delete data/vectors/*.hex to regenerate everything
        }
        let xsd_path = java_schemas.join(xsd);
        let mut args = vec![xsd_path.as_os_str(), "lax".as_ref(), "noheader".as_ref(), xml.as_os_str()];
        if fragment {
            args.push("fragment".as_ref());
        }
        let out = java_run(&cp, "EncodeVector", &args)
        .map_err(|e| format!("{name}: {e}"))?;
        std::fs::write(&dest, out.trim().to_string() + "\n")
            .map_err(|e| e.to_string())?;
        println!("wrote data/vectors/{name}.hex");
    }
    Ok(())
}

/// Compares, for every message, the grammar `exi-gen` derives with the one EXIficient builds
/// from the same schema (event codes, datatypes, nesting).
pub fn grammars() -> Result<(), String> {
    let schemas = workspace_root().join("data/schemas/iso15118-20");
    let (_, cp) = java_env()?;
    let java_schemas = java_schemas()?;
    let mut failed = 0;
    for group in exi_gen::GROUPS {
        for (ns, file, root) in exi_gen::message_roots(group, &schemas)? {
            let ours = exi_gen::dump_grammar(group, &schemas, &ns, &root)?;
            let xsd = java_schemas.join(&file);
            let theirs = java_run(&cp, "DumpTree", &[xsd.as_os_str(), root.as_ref()])?;
            if ours == theirs {
                println!("same  {root}");
            } else {
                failed += 1;
                println!("DIFF  {root}");
                let (a, b): (Vec<_>, Vec<_>) = (ours.lines().collect(), theirs.lines().collect());
                if let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)) {
                    println!("  exi-gen:   {}\n  EXIficient: {}", a.get(i).unwrap_or(&"<end>"), b.get(i).unwrap_or(&"<end>"));
                }
            }
        }
    }
    if failed > 0 { Err(format!("{failed} grammars differ")) } else { Ok(()) }
}
