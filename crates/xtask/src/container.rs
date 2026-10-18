//! The Linux container for the network tests and the interoperability runs.

use std::process::Command;

use crate::{run, workspace_root};

/// Runs `cargo test` for the Linux-only tests (network namespaces, veth pairs, raw sockets)
/// in a privileged container, so they work from macOS through Docker/Colima. The target
/// directory and the cargo registry live in named volumes: the host's `target` holds macOS
/// builds (and, in `target/sandbox`, the artifacts of the runs). Inside, `developer` builds and the test binaries run as root through sudo.
pub fn linux_test(cargo_args: Vec<String>) -> Result<(), String> {
    let mut cmd = linux_container(&[])?;
    // `own-volumes` first: volumes made by an older image belong to root.
    cmd.args(["bash", "-c", r#"own-volumes && cargo build --quiet --workspace --bins && exec cargo "$@""#, "cargo", "test"]);
    if cargo_args.is_empty() {
        cmd.args(["--workspace"]);
    } else {
        cmd.args(&cargo_args);
    }
    run(&mut cmd)
}

/// The Linux container for `linux_test`, `interop` and `web`: builds the image and returns the
/// `docker run` command up to the program to run. `publish` are `-p` arguments.
fn linux_container(publish: &[String]) -> Result<Command, String> {
    const IMAGE: &str = "evcc-rs-linux-test";
    let root = workspace_root();
    run(Command::new("docker")
        .args(["build", "--quiet", "--tag", IMAGE, "--file"])
        .arg(root.join(".devcontainer/Dockerfile"))
        .arg(root.join(".devcontainer")))?;
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--privileged"]);
    for p in publish {
        cmd.args(["--publish", p]);
    }
    cmd.arg("--volume")
        .arg(format!("{}:/workspaces/evcc-rs", root.display()))
        .args(["--volume", "evcc-rs-target-v2:/target", "--volume", "evcc-rs-cargo-v2:/usr/local/cargo/registry"])
        // The artifacts of the runs (logs, settings, captures) go to the host's `target/sandbox`,
        // not into the build volume, so they can be looked at (Wireshark) from the host.
        .args(["--env", "SANDBOX_ARTIFACTS=/workspaces/evcc-rs/target/sandbox"])
        .args(["--workdir", "/workspaces/evcc-rs", IMAGE]);
    Ok(cmd)
}

/// Clippy with warnings as errors, as CI runs it: for this machine, and, from a machine that is not Linux, once more in
/// the Linux container, since a good part of the code (sockets, namespaces, SLAC) is compiled on Linux only.
pub fn lint() -> Result<(), String> {
    let clippy = ["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"];
    run(Command::new("cargo").args(clippy))?;
    if cfg!(target_os = "linux") {
        return Ok(());
    }
    let mut cmd = linux_container(&[])?;
    cmd.args(["bash", "-c", "own-volumes && cargo clippy --workspace --all-targets --locked -- -D warnings"]);
    run(&mut cmd)
}

/// Runs the interoperability scenarios (`crates/sims/scenarios/scenarios/interop/`) in the Linux
/// container: the simulated vehicle against an independent charger implementation. Names
/// select scenarios (`iso15118-python`, `iso15118-python-tls`); without any all of them run.
pub fn interop(names: Vec<String>) -> Result<(), String> {
    let dir = "crates/sims/scenarios/scenarios/interop";
    let files: Vec<String> = if names.is_empty() {
        vec![format!("{dir}/*.toml")]
    } else {
        names.iter().map(|n| format!("{dir}/{n}.toml")).collect()
    };
    let mut cmd = linux_container(&[])?;
    // `own-volumes` first: volumes made by an older image belong to root. Only the runner
    // runs as root (it creates the namespaces); the participants drop to the invoking user.
    cmd.args(["bash", "-c", r#"own-volumes && cargo build --quiet --workspace --bins && exec sudo -E /target/debug/scenario $@"#, "interop"])
        .args(files);
    run(&mut cmd)
}

/// Starts the web UI in the Linux container (or, inside the dev container, directly), published on `127.0.0.1:PORT` of this machine
/// (default 8080); scenarios need the namespaces. Runs until interrupted. The server runs as
/// the container's user and starts each run through `sudo`.
pub fn web(args: Vec<String>) -> Result<(), String> {
    let port = match args.as_slice() {
        [] => "8080".to_string(),
        [p] if p.parse::<u16>().is_ok() => p.clone(),
        _ => return Err("usage: cargo xtask web [PORT]".into()),
    };
    // Already in the Linux container (the dev container): no Docker here, serve directly.
    if std::path::Path::new("/.dockerenv").exists() {
        run(Command::new("cargo").args(["build", "--quiet", "--workspace", "--bins"]))?;
        let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into());
        let mut cmd = Command::new(std::path::Path::new(&target).join("debug/evcc-web"));
        cmd.args(["--listen", &format!("0.0.0.0:{port}"), "--work"]).arg(std::path::Path::new(&target).join("web"));
        return run(&mut cmd);
    }
    let mut cmd = linux_container(&[format!("127.0.0.1:{port}:8080")])?;
    cmd.args(["bash", "-c", r#"own-volumes && cargo build --quiet --workspace --bins && exec /target/debug/evcc-web --listen 0.0.0.0:8080 --work /workspaces/evcc-rs/target/web"#]);
    run(&mut cmd)
}
