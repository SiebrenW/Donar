//! The end-to-end tests: every file in `scenarios/` is run in the sandbox (Linux, root; the
//! dev container's test runner provides both) and must pass. A failing scenario can be run
//! by hand with `sudo target/debug/scenario crates/sims/scenarios/scenarios/NAME.toml`.
//! The external-partner scenarios in `scenarios/interop/` are run by `cargo xtask interop`.

use std::path::{Path, PathBuf};

use scenarios::Scenario;

fn directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios")
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_scenario_file_loads() {
    let all: Vec<PathBuf> = [directory(), directory().join("interop")].iter().flat_map(|d| files(d)).collect();
    assert!(all.len() >= 8, "{all:?}");
    for file in all {
        Scenario::load(&file).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[cfg(target_os = "linux")]
mod run {
    use super::*;

    fn run(name: &str) {
        let scenario = Scenario::load(&directory().join(format!("{name}.toml"))).unwrap();
        // A scenario that hangs would hold the whole run: the runner has its own timeouts,
        // this ends the process if they fail too.
        let limit = std::time::Duration::from_secs(scenario.timeout_s + 60);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            // A dropped sender means the scenario is over.
            if done_rx.recv_timeout(limit) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
                eprintln!("a scenario is still running after {limit:?}: ending the run");
                std::process::exit(101);
            }
        });
        let report = scenarios::runner::run(&scenario).expect("the sandbox needs root and a user (sudo)");
        drop(done_tx);
        watchdog.join().unwrap();
        assert!(report.passed(), "scenario {name} failed:\n{}", report.describe());
    }

    macro_rules! scenario_tests {
        ($($test:ident => $file:literal,)*) => { $(#[test] fn $test() { run($file) })* };
    }

    scenario_tests! {
        dc_plain => "dc-plain",
        dc_slac => "dc-slac",
        slac_too_far => "slac-too-far",
        dc_tls_slac => "dc-tls-slac",
        dc_tls_ed448 => "dc-tls-ed448",
        dc_delay => "dc-delay",
        charger_ends_charge => "charger-ends-charge",
    }

    #[test]
    fn every_scenario_file_has_a_test() {
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios.rs")).unwrap();
        for file in files(&directory()) {
            let stem = file.file_stem().unwrap().to_str().unwrap();
            assert!(source.contains(&format!("\"{stem}\"")), "no test for scenarios/{stem}.toml");
        }
    }
}
