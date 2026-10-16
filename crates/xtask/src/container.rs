//! The Linux container for the network tests and the interoperability runs.

use std::process::Command;

use crate::{run, workspace_root};

/// Runs `cargo test` for the Linux-only tests (network namespaces, veth pairs, raw sockets)
/// in a privileged container, so they work from macOS through Docker/Colima. The target
/// directory and the cargo registry live in named volumes: the host's `target` holds macOS
/// builds. Inside, `developer` builds and the test binaries run as root through sudo.
pub fn linux_test(cargo_args: Vec<String>) -> Result<(), String> {
    let mut cmd = linux_container()?;
    // `own-volumes` first: volumes made by an older image belong to root.
    cmd.args(["bash", "-c", r#"own-volumes && exec cargo "$@""#, "cargo", "test"]);
    if cargo_args.is_empty() {
        cmd.args(["--workspace"]);
    } else {
        cmd.args(&cargo_args);
    }
    run(&mut cmd)
}

/// The Linux container for `linux_test` and `interop`: builds the image and returns the
/// `docker run` command up to the program to run.
fn linux_container() -> Result<Command, String> {
    const IMAGE: &str = "evcc-rs-linux-test";
    let root = workspace_root();
    run(Command::new("docker")
        .args(["build", "--quiet", "--tag", IMAGE, "--file"])
        .arg(root.join(".devcontainer/Dockerfile"))
        .arg(root.join(".devcontainer")))?;
    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm", "--privileged"])
        .arg("--volume")
        .arg(format!("{}:/workspaces/evcc-rs", root.display()))
        .args(["--volume", "evcc-rs-target-v2:/target", "--volume", "evcc-rs-cargo-v2:/usr/local/cargo/registry"])
        // Lets `INTEROP_SHOW_PARTNER_LOG=1 cargo xtask interop` print the partner's log.
        .args(["--env", "INTEROP_SHOW_PARTNER_LOG"])
        .args(["--workdir", "/workspaces/evcc-rs", IMAGE]);
    Ok(cmd)
}

/// Runs the `evcc-sim` command against an independent charger implementation (default partner:
/// `iso15118-python`; also `iso15118-python-tls`) in the Linux container, in a sandbox
/// (`crates/sims/scenarios/src/bin/interop.rs`). The partner's defaults for `evcc-sim` (TLS or
/// not, certificates, inlet) come first, further arguments after.
pub fn interop(args: Vec<String>) -> Result<(), String> {
    let mut cmd = linux_container()?;
    // `own-volumes` first: volumes made by an older image belong to root. Only the driver runs
    // as root (it creates the namespaces); the participants drop to the invoking user.
    cmd.args(["bash", "-c", r#"own-volumes && cargo build --quiet -p evcc-sim -p scenarios --bins && exec sudo -E /target/debug/interop "$@""#, "interop"])
        .args(args);
    run(&mut cmd)
}
