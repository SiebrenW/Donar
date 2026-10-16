//! `scenario FILE...`: runs scenario files in the sandbox and reports pass or fail for each.
//! Needs root and a user to drop to (sudo), and the simulators built next to it. The logs of
//! every run are in its artifacts directory, which is printed.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("scenario: needs Linux network namespaces (use `cargo xtask linux-test` or `cargo xtask interop`)");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::path::Path;
    use std::process::ExitCode;

    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: scenario FILE.toml...");
        return ExitCode::from(2);
    }
    let mut failed = 0;
    for file in &files {
        let scenario = match scenarios::Scenario::load(Path::new(file)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        eprintln!("running {} ({file})", scenario.name);
        match scenarios::runner::run(&scenario) {
            Ok(report) if report.passed() => println!("PASS {}  (artifacts: {})", scenario.name, report.artifacts.display()),
            Ok(report) => {
                failed += 1;
                println!("FAIL {}\n{}", scenario.name, report.describe());
            }
            Err(e) => {
                failed += 1;
                println!("ERROR {}: {e}", scenario.name);
            }
        }
    }
    if failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
