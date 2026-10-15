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
        Some("setup") => {
            let force = args.any(|a| a == "--force");
            setup(force).and_then(|()| generate(force))
        }
        Some("gen") => generate(args.any(|a| a == "--force")),
        Some("vectors") => vectors(),
        Some("grammars") => grammars(),
        Some("linux-test") => linux_test(args.collect()),
        _ => {
            eprintln!("usage: cargo xtask setup [--force] | gen [--force] | vectors | grammars | linux-test [cargo test args]");
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

/// Downloads EXIficient and compiles the helper programs in `xtask/java`; returns the class
/// directory and the class path.
fn java_env() -> Result<(PathBuf, String), String> {
    let root = workspace_root();
    let lib = root.join("vectors/lib");
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
    let classes = root.join("vectors/classes");
    std::fs::create_dir_all(&classes).map_err(|e| e.to_string())?;
    let cp = format!("{}/*", lib.display());
    let mut sources: Vec<PathBuf> = std::fs::read_dir(root.join("xtask/java"))
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
    let (from, to) = (root.join("schemas/iso15118-20"), root.join("vectors/schemas"));
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

/// Encodes every XML in `xtask/vectors` with EXIficient into `vectors/<name>.hex`.
/// These are the independent oracle for the codecs; they are gitignored.
fn vectors() -> Result<(), String> {
    let root = workspace_root();
    let (_, cp) = java_env()?;
    let java_schemas = java_schemas()?;
    let manifest = std::fs::read_to_string(root.join("xtask/vectors/manifest.txt"))
        .map_err(|e| e.to_string())?;
    for line in manifest.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let mut it = line.split_whitespace();
        let (name, xsd) = (it.next().unwrap(), it.next().ok_or("manifest: missing xsd")?);
        let xml = root.join(format!("xtask/vectors/{name}.xml"));
        let dest = root.join(format!("vectors/{name}.hex"));
        let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        if mtime(&dest).is_some_and(|h| mtime(&xml).is_some_and(|x| h >= x)) {
            continue; // up to date; delete vectors/*.hex to regenerate everything
        }
        let xsd_path = java_schemas.join(xsd);
        let out = java_run(
            &cp,
            "EncodeVector",
            &[xsd_path.as_os_str(), "lax".as_ref(), "noheader".as_ref(), xml.as_os_str()],
        )
        .map_err(|e| format!("{name}: {e}"))?;
        std::fs::write(&dest, out.trim().to_string() + "\n")
            .map_err(|e| e.to_string())?;
        println!("wrote vectors/{name}.hex");
    }
    Ok(())
}

/// Runs `cargo test` for the Linux-only tests (network namespaces, veth pairs, raw sockets)
/// in a privileged container, so they work from macOS through Docker/Colima. The target
/// directory and the cargo registry live in named volumes: the host's `target` holds macOS
/// builds. Inside, `developer` builds and the test binaries run as root through sudo.
fn linux_test(cargo_args: Vec<String>) -> Result<(), String> {
    const IMAGE: &str = "evcc-rs-linux-test";
    let root = workspace_root();
    run(Command::new("docker")
        .args(["build", "--quiet", "--tag", IMAGE, "--file"])
        .arg(root.join("docker/linux-test.Dockerfile"))
        .arg(root.join("docker")))?;
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--privileged"])
        .arg("--volume")
        .arg(format!("{}:/work", root.display()))
        .args(["--volume", "evcc-rs-target:/target", "--volume", "evcc-rs-cargo:/usr/local/cargo/registry"])
        .args(["--workdir", "/work", IMAGE])
        // `own-volumes` first: volumes made by an older image belong to root.
        .args(["bash", "-c", r#"own-volumes && exec cargo "$@""#, "cargo", "test"]);
    if cargo_args.is_empty() {
        cmd.args(["--workspace"]);
    } else {
        cmd.args(&cargo_args);
    }
    run(&mut cmd)
}

fn run(cmd: &mut Command) -> Result<(), String> {
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{cmd:?} exited with {s}")),
        Err(e) => Err(format!("cannot run {cmd:?}: {e}")),
    }
}

/// Crate directory that holds the generated code of an `exi-gen` group.
fn group_dir(group: &exi_gen::Group) -> PathBuf {
    workspace_root().join("codecs").join(group.name)
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Generates the codecs (gitignored `src/generated` of each codec crate) from the XSDs.
/// Skipped when the output is newer than every schema and than this binary, which carries
/// the generator.
fn generate(force: bool) -> Result<(), String> {
    let schemas = workspace_root().join("schemas/iso15118-20");
    let exe = std::env::current_exe().ok().and_then(|p| mtime(&p));
    for group in exi_gen::GROUPS {
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

/// Compares, for every message, the grammar `exi-gen` derives with the one EXIficient builds
/// from the same schema (event codes, datatypes, nesting).
fn grammars() -> Result<(), String> {
    let schemas = workspace_root().join("schemas/iso15118-20");
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
