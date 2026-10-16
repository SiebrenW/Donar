# Getting started

`evcc-rs` is an EV communication controller (EVCC) for ISO 15118-20 with a simulated charger (EVSE) to test against. This page gets you from a fresh clone to a green test run. The plan and the status of the standard's requirements are in [PLAN.md](PLAN.md).

## Two ways to work

| | Where | What runs |
|---|---|---|
| **Dev container** (recommended) | Linux container, started by VS Code or the `devcontainer` CLI | everything, including the network namespace tests |
| **Local** | your macOS or Linux machine | everything except the Linux-only network tests, which you run through `cargo xtask linux-test` (needs Docker) |

The Linux-only tests create network namespaces and virtual Ethernet pairs and open raw packet sockets (SLAC). They need root and a Linux kernel, hence the container.

## Dev container

Prerequisites: Docker (Docker Desktop, Colima or similar) in your `PATH`, and an editor with dev container support: VS Code with the Dev Containers extension, [Zed](https://zed.dev/docs/dev-containers), or the [`devcontainer` CLI](https://github.com/devcontainers/cli).

1. Open the folder in your editor and accept the offer to open it in the container (VS Code: **Reopen in Container**; Zed: **Open in Container** in the prompt; CLI: `devcontainer up --workspace-folder .`).
2. The first start builds the image from `.devcontainer/Dockerfile` (Rust, JDK, iproute2) and then runs `cargo xtask setup && cargo xtask vectors`. That downloads the ISO 15118-20 schemas, generates the codecs and builds the reference vectors. It takes a few minutes and needs network access.
3. In a terminal inside the container:

   ```sh
   cargo test --workspace
   ```

Zed ignores the `customizations.vscode` block (extensions, settings) of `devcontainer.json` and brings its own rust-analyzer. The setup has been tried with VS Code's settings format and the plain Docker commands; the Zed route is untested, in particular whether Zed passes `--privileged` from `runArgs`. If the namespace tests fail with "Operation not permitted" in a Zed container, run them from a normal terminal with `cargo xtask linux-test`.

The repository is mounted at `/workspaces/evcc-rs` in the container, both by the dev container and by `cargo xtask linux-test`; keep it that way, because cargo bakes the path into build outputs (`env!("CARGO_MANIFEST_DIR")`) that live in a shared volume. You work as the user `developer` (UID 1000, which an editor may change to your own UID) with passwordless `sudo`. Only the test binaries run as root: the image sets `CARGO_TARGET_*_RUNNER` to `.devcontainer/test-runner.sh`, which starts programs from cargo's `deps` directory through `sudo`, so the namespace and raw-socket tests get their privileges while building and `cargo xtask` stay unprivileged. The container is **privileged** (the namespace tests need it), so use it on a machine and Docker VM you trust. Build output lives in the named volume `evcc-rs-target-v2` (`CARGO_TARGET_DIR=/target`), so it never mixes with a `target/` directory made on macOS.

## Local setup

Prerequisites:

- Rust (stable, edition 2024; `rustup` is the easiest way) with `rustfmt`, which the generator uses.
- A C toolchain and the OpenSSL development files for `evcc-linux`: on macOS `brew install openssl@3` (set `OPENSSL_DIR=$(brew --prefix openssl@3)` if the build does not find it), on Debian/Ubuntu `apt install libssl-dev pkg-config`.
- `curl` and a JDK (`java` and `javac`; on macOS `brew install openjdk`) for the one-time setup below. The JDK is only used to produce reference vectors with EXIficient.

One-time setup:

```sh
cargo xtask setup     # download the schemas, generate the codecs
cargo xtask vectors   # build the EXIficient reference vectors
```

Both skip what is already there; `setup --force` fetches and generates again. The schemas, generated code and vectors are not in git (the schemas are ISO's; the rest is derived), so a fresh clone needs this step before `cargo test` works.

## Running the tests

```sh
cargo test --workspace                    # everything that runs on this machine
cargo test -p evcc-core                   # one crate
cargo test -p scenarios --test tls        # one test file
cargo bench -p iso15118-20                # codec throughput over the reference vectors
cargo xtask linux-test                    # the whole workspace inside the Linux container
cargo xtask linux-test -p scenarios --test scenarios   # only the scenarios
```

`cargo xtask linux-test [cargo test arguments]` builds the image, starts a privileged container with the repository mounted and runs `cargo test` in it. It is what you use from macOS without the dev container. In the dev container you run `cargo test` directly.

### Scenarios

A scenario is a TOML file that says who runs against whom, with which settings, and what must hold afterwards (`crates/sims/scenarios/scenarios/*.toml`, the format is documented in `crates/sims/scenarios/src/scenario.rs`). The runner puts a charger and a vehicle in two network namespaces joined by a cable, starts them as processes and checks exit statuses and log lines. `tests/scenarios.rs` runs every file as a test (`cargo xtask linux-test`); a single one by hand:

```sh
cargo xtask linux-test -p scenarios --test scenarios dc_tls_slac
```

Only the runner is root (it creates the namespaces); `evse-sim` and `evcc-sim` run as you, and their logs, settings and generated certificates are in the artifacts directory the run prints (`target/sandbox/<run>/`, owned by you). Both simulators are ordinary commands with a settings file, so a participant can also be started by hand: `evse-sim --interface IF --address ADDR --config FILE`, `evcc-sim --interface IF --config FILE` (`--help`-style usage at the top of `crates/sims/{evse-sim,evcc-sim}/src/main.rs`).

### Web UI

```sh
cargo xtask web          # http://127.0.0.1:8080/ (an argument picks another port)
```

Starts `evcc-web` in the Linux container and publishes it on localhost. Pick a scenario, change the settings of the vehicle and the charger in the forms (a value you set is highlighted, × resets it), press **Run** and follow both logs live; the verdict is the runner's `PASS` or `FAIL`. The server runs as the container's user and starts each run through `sudo`; the logs and settings of a run stay in `target/web/run-N/` in the container's build volume. Inside the dev container run `cargo run -p evcc-web` after `cargo build --workspace --bins`. It is meant for your own machine: no authentication.

#### Simulation tab

*Simulation* opens a sandbox with a charger and runs a vehicle session in it one step at a time: **Open simulation** (TLS, SLAC and the PKI are chosen here), edit the charge parameters of both sides (left and right tables: the value for the *next* session beside the one *in use*), **New session**, then pick a phase and press **Advance**. The vehicle waits before each phase until told to go on, with protocol timers frozen; measurements (battery voltage, SoC, the charger's voltage, current and maximum power) can be changed at any time, or left to the model with Auto. **Stop charging** advances to the stop; **Close simulation** removes the namespaces. `?static` in the URL skips the live connection, for screenshots.

### Interoperability with another charger implementation

```sh
cargo xtask interop                       # all scenarios in scenarios/interop/
cargo xtask interop iso15118-python-tls   # one of them: TLS 1.3 with mutual authentication
```

These scenarios run `evcc-sim` against an independent ISO 15118-20 charger in another namespace: the open-source [`iso15118`](https://pypi.org/project/iso15118/) Python library, installed in the container. Output is `PASS` or `FAIL` with both logs. The partner offers only the `Extended` inlet, so its scenarios set `connector = "extended"`; its test certificates are secp256r1 (the ISO 15118-2 profile), so the runner switches the vehicle's `backwards_compatible` on for them.

### What the tests cover

| Crate / file | What it checks | Needs Linux root |
|---|---|---|
| `crates/codec/exi`, `.../app-protocol`, `.../iso15118-20` | The generated EXI codec byte for byte against EXIficient vectors, in both directions; truncated and bit-flipped input; message sizes | no |
| `codec/v2gtp`, `codec/homeplug` | V2GTP and SDP framing; SLAC message wire formats | no |
| `crates/evcc/core/tests/session_flow.rs` | The session machine message by message: setup, authorization, services, charge parameters, schedule exchange, cable check, pre-charge, power delivery, charge loop, welding detection, session stop, timeouts and the ignore policy | no |
| `crates/evcc/core/tests/slac_flow.rs` | The SLAC state machine against the simulated charger in virtual time: retries, thresholds, several chargers, lost messages | no |
| `crates/sims/scenarios/tests/loopback.rs` | The session over real TCP on localhost against the simulator | no |
| `crates/sims/scenarios/tests/tls.rs` | TLS 1.3 with mutual authentication, P-521 and Ed448 certificates, refusal of unknown CAs and wrong curves | no |
| `crates/sims/scenarios/tests/sdp.rs` | SDP discovery over UDP on the IPv6 loopback | no |
| `crates/sims/web/tests/web.rs` | The web server over HTTP: scenario list, schemas, refusal of bad scenarios; a run with its logs streamed (Linux) | the run: yes |
| `crates/sims/scenarios/tests/scenarios.rs` | Every scenario file in two network namespaces: multicast SDP, SLAC on a raw packet socket (matched and too far away), TLS 1.3 with P-521 and Ed448 certificates, a delayed cable, a charger that ends the charge; plus a check that all scenario files load | **yes** (Linux only; the loading check runs everywhere) |

Tests that need root are compiled only on Linux, so `cargo test --workspace` on macOS simply does not contain them.

## Layout

```
crates/
  codec/                           wire formats (no state machine, no I/O)
    exi, app-protocol, iso15118-20   the EXI runtime and the generated message codecs
    exi-gen                          the code generator (XSD to Rust)
    v2gtp, homeplug                  V2GTP/SDP framing, SLAC messages
  evcc/
    core                             the sans-IO session, SDP and SLAC machines (no_std)
    linux                            host adapter: TCP, TLS (OpenSSL), UDP/SDP, raw sockets (SLAC)
  sims/
    evse-sim                         a simulated charger
    evcc-sim                         a simulated vehicle and the `evcc-sim` command
    web                              `evcc-web`: start scenarios, edit settings, follow the logs in a browser
    sandbox                          network namespaces joined by virtual cables; participants run as processes
    scenarios                        scenario files, the runner (`scenario` command), partners, end-to-end tests
  xtask                            `cargo xtask ...` (setup, gen, vectors, linux-test, interop, web)
.devcontainer/                     the Linux test environment (also used by `cargo xtask linux-test`)
data/                              downloaded schemas and reference vectors (not in git)
standards/                         your copies of the standards (not in git)
```

## Troubleshooting

- **`missing .../data/vectors/*.hex`**: run `cargo xtask setup && cargo xtask vectors`.
- **OpenSSL not found (macOS)**: `export OPENSSL_DIR=$(brew --prefix openssl@3)`.
- **`cargo xtask linux-test` cannot reach Docker**: start your Docker VM (`colima start` or Docker Desktop). With Colima the repository must be below your home directory, which it shares by default.
- **Namespace tests fail with "Operation not permitted"**: the container is not privileged; use `cargo xtask linux-test` or the dev container as configured.
- **"Permission denied" writing in the repository from the container (Linux host)**: the image's `developer` has UID 1000; the dev container adjusts it to your UID (`updateRemoteUserUID`), `cargo xtask linux-test` does not. If your UID is not 1000, run `docker run --user $(id -u) ...` yourself or use the dev container. On macOS with Colima or Docker Desktop this does not occur.
- **Build output owned by root after an older image**: the container runs `own-volumes` (a `chown` of the two volumes) on start, in the dev container and in `cargo xtask linux-test`.
- **Tests fail in the container with a path like `/work/...` or `Permission denied` from `cargo xtask`**: the build volume was made by an earlier layout with the repository at another path. Remove the volumes (`docker volume rm evcc-rs-target-v2 evcc-rs-cargo-v2` with no container using them) and rebuild.
- **Leftover namespaces after an aborted run**: `ip netns list` inside the container shows them; they vanish when the container ends.
