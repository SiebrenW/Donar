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
        Some("vectors") => vectors(),
        _ => {
            eprintln!("usage: cargo xtask setup [--force] | vectors");
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

/// Encodes every XML in `xtask/vectors` with EXIficient into `vectors/<name>.hex`.
/// These are the independent oracle for the codecs; they are gitignored.
fn vectors() -> Result<(), String> {
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
    run(Command::new(jdk_tool("javac"))
        .args(["-cp", &cp, "-d"])
        .arg(&classes)
        .arg(root.join("xtask/java/EncodeVector.java")))?;
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
        // Xerces fetches the XMLSchema DTD that xmldsig-core-schema.xsd names from w3.org,
        // which rate-limits; retry a few times.
        let mut attempt = 0;
        let out = loop {
            let out = Command::new(jdk_tool("java"))
                .args(["-cp", &format!("{}:{cp}", classes.display())])
                .arg("EncodeVector")
                .arg(root.join("schemas/iso15118-20").join(xsd))
                .args(["lax", "noheader"])
                .arg(&xml)
                .output()
                .map_err(|e| e.to_string())?;
            attempt += 1;
            if out.status.success() || attempt == 5 {
                break out;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        };
        if !out.status.success() {
            return Err(format!("{name}: {}", String::from_utf8_lossy(&out.stderr)));
        }
        let hex = String::from_utf8_lossy(&out.stdout);
        std::fs::write(&dest, hex.trim().to_string() + "\n")
            .map_err(|e| e.to_string())?;
        println!("wrote vectors/{name}.hex");
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{cmd:?} exited with {s}")),
        Err(e) => Err(format!("cannot run {cmd:?}: {e}")),
    }
}
