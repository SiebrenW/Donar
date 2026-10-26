# Getting started

Donar is a toolkit for the vehicle-to-charger high level communication (ISO 15118-20, ISO 15118-2, DIN SPEC 70121, SLAC): an EV communication controller (EVCC), a charger side (EVSE/SECC) and simulators to test them against each other. This page gets you from a fresh clone to a green test run. The plan and the status of the standard's requirements are in [PLAN.md](../PLAN.md).

## Linux only

Donar targets Linux: vehicles and chargers do not run macOS or Windows, so there is no other platform to keep working. Develop in the dev container (below), which is a Linux environment with everything installed; on a Mac or Windows machine that is the only supported way. On a Linux machine you may work natively (see Local setup). The tests create network namespaces and virtual Ethernet pairs and open raw packet sockets (SLAC); they need root and a Linux kernel.

## Dev container

Prerequisites: Docker (Docker Desktop, Colima or similar) in your `PATH`, and an editor with dev container support: VS Code with the Dev Containers extension, [Zed](https://zed.dev/docs/dev-containers), or the [`devcontainer` CLI](https://github.com/devcontainers/cli).

1. Open the folder in your editor and accept the offer to open it in the container (VS Code: **Reopen in Container**; Zed: **Open in Container** in the prompt; CLI: `devcontainer up --workspace-folder .`).
2. The first start builds the image from `.devcontainer/Dockerfile` (Rust, JDK, iproute2) and then runs `cargo xtask setup && cargo xtask vectors`. That downloads the schemas, regenerates the codecs (they are committed, so this changes nothing unless the generator or a schema did) and builds the reference vectors. It takes a few minutes and needs network access.
3. In a terminal inside the container:

   ```sh
   cargo test --workspace
   ```

After a change to `.devcontainer/Dockerfile` (a new tool), rebuild the dev container (VS Code: **Rebuild Container**; CLI: `devcontainer up --remove-existing-container`): a container made before keeps the old tools. A scenario run without `tcpdump` goes on without its `capture.pcap` and says so in its output.

Zed ignores the `customizations.vscode` block (extensions, settings) of `devcontainer.json` and brings its own rust-analyzer. The setup has been tried with VS Code's settings format and the plain Docker commands; the Zed route is untested, in particular whether Zed passes `--privileged` from `runArgs`. If the namespace tests fail with "Operation not permitted" in a Zed container, run them from a normal terminal with `cargo xtask linux-test`.

The repository is mounted at `/workspaces/evcc-rs` in the container, both by the dev container and by `cargo xtask linux-test`; keep it that way, because cargo bakes the path into build outputs (`env!("CARGO_MANIFEST_DIR")`) that live in a shared volume. You work as the user `developer` (UID 1000, which an editor may change to your own UID) with passwordless `sudo`. Only the test binaries run as root: the image sets `CARGO_TARGET_*_RUNNER` to `.devcontainer/test-runner.sh`, which starts programs from cargo's `deps` directory through `sudo`, so the namespace and raw-socket tests get their privileges while building and `cargo xtask` stay unprivileged. The container is **privileged** (the namespace tests need it), so use it on a machine and Docker VM you trust. Build output lives in the named volume `evcc-rs-target-v2` (`CARGO_TARGET_DIR=/target`), so it never mixes with a `target/` directory made on the host.

## Local setup (Linux)

Prerequisites:

- Rust (stable, edition 2024; `rustup` is the easiest way) with `rustfmt`, which the generator uses.
- A C toolchain and the OpenSSL development files for `evcc-linux`: on Debian/Ubuntu `apt install libssl-dev pkg-config`.
- `curl` and a JDK (`java` and `javac`) for the one-time setup below. The JDK is only used to produce reference vectors with EXIficient.

One-time setup:

```sh
cargo xtask setup     # download the schemas, regenerate the codecs
cargo xtask vectors   # build the EXIficient reference vectors
```

Both skip what is already there; `setup --force` fetches and generates again. The generated codecs are in git (`crates/codec/*/src/generated/`; CI regenerates them and fails when they differ from the committed ones). The schemas and the vectors are not (the schemas are ISO's, the vectors are derived), so a fresh clone builds, but the codec tests that compare with the vectors need this step.

## Running the tests

```sh
cargo test --workspace                    # everything
cargo test -p evcc-core                   # one crate
cargo test -p scenarios --test tls        # one test file
cargo bench -p iso15118-20                # codec throughput over the reference vectors
cargo xtask linux-test                    # the whole workspace inside the Linux container
cargo xtask linux-test -p scenarios --test scenarios   # only the scenarios
```

`cargo xtask linux-test [cargo test arguments]` builds the image, starts a privileged container with the repository mounted and runs `cargo test` in it. It is the way in from a terminal on the host, outside the dev container. In the dev container you run `cargo test` directly.

### Scenarios

Every scenario in `scenarios/` starts with SLAC (`slac = true` for the vehicle, `slac_responder = true` for the charger; `[secc.settings.slac] ac = true` for AC chargers), as a real bring-up does; a test checks it, and a scenario that must go without lists itself, with the reason, in `WITHOUT_SLAC` in `tests/scenarios.rs` (`basic-charging` does: its charger offers a nominal duty cycle, `nominal_duty_percent` in the settings of `evse-sim`, which is basic charging with no SLAC and no high level communication). The external-partner scenarios in `scenarios/interop/` are not changed.

A scenario is a TOML file that says who runs against whom, with which settings, and what must hold afterwards (`crates/sims/scenarios/scenarios/*.toml`, the format is documented in `crates/sims/scenarios/src/scenario.rs`). The runner puts a charger and a vehicle in two network namespaces joined by a cable, starts them as processes and checks exit statuses and log lines. `tests/scenarios.rs` runs every file as a test (`cargo xtask linux-test`); a single one by hand:

```sh
cargo xtask linux-test -p scenarios --test scenarios dc_tls_slac
```

Only the runner is root (it creates the namespaces); `evse-sim` and `evcc-sim` run as you, and their logs, settings and generated certificates are in the artifacts directory the run prints (`target/sandbox/<time>-<pid>-<n>/` in the repository, also when the tests run in the container, owned by you; old runs are not removed). Where that is: `$SANDBOX_ARTIFACTS`, else `target/sandbox` of the repository when it is mounted at `/workspaces/evcc-rs`, else `$CARGO_TARGET_DIR/sandbox` (`sandbox::artifacts_root`), so the host sees it whichever shell started the run. Every run also leaves `capture.pcap` there: `tcpdump` on the cable from before the first packet (SLAC, SDP, TCP). In a TLS run the vehicle simulator discloses the session secrets, one UDP packet per key log line (source port 49152 or higher, to `ff02::1` port 50000, `tls.disclose_secrets` in the settings of `evcc-sim`, `disclose_secrets` in those of `evse-sim`; on only in the scenario runner, and only for test platforms), which is what the [dSPACE V2Gshark](https://github.com/dspace-group/dsV2Gshark) plugin for Wireshark reads to decrypt the session and decode the V2G messages. Without the plugin the same lines work as a key log: `tshark -r capture.pcap -Y 'udp.dstport==50000' -T fields -e data.data | while read h; do echo $h | xxd -r -p; echo; done > keys.log`, then `-o tls.keylog_file:keys.log`. A run without `tcpdump` goes on without a capture. Both simulators are ordinary commands with a settings file, so a participant can also be started by hand: `evse-sim --interface IF --address ADDR --config FILE`, `evcc-sim --interface IF --config FILE` (`--help`-style usage at the top of `crates/sims/{evse-sim,evcc-sim}/src/main.rs`).

### Web UI

```sh
cargo xtask web          # http://127.0.0.1:8080/ (an argument picks another port)
```

Starts `evcc-web` in the Linux container and publishes it on localhost. Pick a scenario, change the settings of the vehicle and the charger in the forms (a value you set is highlighted, × resets it), press **Run** and follow both logs live; the verdict is the runner's `PASS` or `FAIL`. The server runs as the container's user and starts each run through `sudo`; the logs and settings of a run are in `target/sandbox/web/run-N/` of the repository (the artifacts root above), so they can be opened from the host. Inside the dev container the same command serves directly (no Docker, port 8080 is forwarded by the editor); do not use `sudo`, the server asks for it itself when it starts a run. It is meant for your own machine: no authentication.

#### Simulation tab

*Simulation* opens a sandbox with a charger and runs a vehicle session in it one step at a time: **Open simulation** (TLS, SLAC and the PKI are chosen here), edit the charge parameters of both sides (left and right tables: the value for the *next* session beside the one *in use*), **New session**, then pick a phase and press **Step to** (run up to the start of the phase and hold before its first message) or **Run in** (go into the phase and stay in it: repeated messages such as the cable check polls or the charge loop keep flowing until you move on). The vehicle waits before each phase it was not sent to, with protocol timers frozen; the limits of both sides can be changed while it runs (the *in use* column, effective from the next message), and so can the measurements (battery voltage, SoC, the charger's voltage, current and maximum power), which can also be left to the model with Auto. **Stop charging** advances to the stop; **Close simulation** removes the namespaces. Below the tables: **Signals**, one graph for the measurements, limits, the vehicle's phase and both sides' states (each against its own range, tick the ones you want; hover for the values); **Messages**, the vehicle's requests and the responses with their codes and the charger's state changes (repeats folded; click a row for what was recorded); **Inject faults**, for the charger a silent mode, an Ongoing that never ends for authorization, cable check or schedule exchange, a response code of your choice for any message, and notifications that ask the vehicle to terminate, pause or renegotiate, and for the vehicle a pause or renegotiation. `?static` in the URL skips the live connection, for screenshots.

### Interoperability with another charger implementation

```sh
cargo xtask interop                       # all scenarios in scenarios/interop/
cargo xtask interop iso15118-python-tls   # one of them: TLS 1.3 with mutual authentication
cargo xtask interop iso15118-python-din   # DIN SPEC 70121 against the same library
```

These scenarios run `evcc-sim` against an independent ISO 15118-20 charger in another namespace: the open-source [`iso15118`](https://pypi.org/project/iso15118/) Python library, installed in the container. Output is `PASS` or `FAIL` with both logs. The partner offers only the `Extended` inlet, so its scenarios set `connector = "extended"`; its test certificates are secp256r1 (the ISO 15118-2 profile), so the runner switches the vehicle's `backwards_compatible` on for them.

### What the tests cover

| Crate / file | What it checks | Needs Linux root |
|---|---|---|
| `crates/codec/exi`, `.../app-protocol`, `.../iso15118-20` | The generated EXI codec byte for byte against EXIficient vectors, in both directions; truncated and bit-flipped input; message sizes | no |
| `codec/v2gtp`, `codec/homeplug` | V2GTP and SDP framing; SLAC message wire formats | no |
| `crates/evcc/core/tests/session_flow.rs` | The session machine message by message: setup, authorization, services, charge parameters, schedule exchange, cable check, pre-charge, power delivery, charge loop, welding detection, session stop, timeouts and the ignore policy | no |
| `crates/evcc/core/tests/slac_flow.rs` | The SLAC state machine against the simulated charger in virtual time: retries, thresholds, several chargers, lost messages, the control pilot gate (5 % starts it, a nominal duty cycle never does, a fault state aborts) | no |
| `crates/sims/scenarios/tests/loopback.rs` | The session over real TCP on localhost against the simulator | no |
| `crates/sims/scenarios/tests/tls.rs` | TLS 1.3 with mutual authentication, P-521 and Ed448 certificates, refusal of unknown CAs and wrong curves | no |
| `crates/sims/scenarios/tests/sdp.rs` | SDP discovery over UDP on the IPv6 loopback | no |
| `crates/sims/web/tests/web.rs` | The web server over HTTP: scenario list, schemas, refusal of bad scenarios; a run with its logs streamed (Linux) | the run: yes |
| `crates/sims/web/tests/browser.rs` | The web page in a headless Chrome or Chromium over the DevTools protocol: it builds, every choice can be made, no script error anywhere (skipped without a browser unless `EVCC_REQUIRE_BROWSER` is set; `CHROME` names the binary; the dev container has Chromium) | no |
| `crates/sims/scenarios/tests/pnc.rs`, `.../certificates.rs` | Plug & Charge (signed `AuthorizationReq`, contract chain, fallback to EIM) and the Annex B certificate profile and OCSP checks, with chains made to the profiles and chains that break one rule | no |
| `crates/sims/scenarios/tests/scenarios.rs` | Every scenario file in two network namespaces: multicast SDP, SLAC on a raw packet socket (matched and too far away), TLS 1.3 with P-521 and Ed448 certificates, a delayed cable, a charger that ends the charge; plus a check that all scenario files load | **yes** |

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `master` and every pull request, on GitHub's hosted runners:

| Job | What | Where it differs from your machine |
|---|---|---|
| `lint` | `cargo clippy --workspace --all-targets -- -D warnings` | run it yourself with `cargo xtask lint` (outside the dev container it runs in the container) |
| `no-std` | the cores, the codecs and `control-pilot` build for `thumbv7em-none-eabihf` | catches `std` that crept into the portable parts; also `cargo xtask features --target thumbv7em-none-eabihf` and `cargo xtask features` (the feature combinations of the cores, warnings as errors) |
| `linux` | `cargo xtask grammars`, then every test of the workspace as root through `.github/ci-runner.sh`: the scenarios in network namespaces (each with its `capture.pcap`), TLS, SLAC, the web page in headless Chrome | the same as `cargo xtask linux-test`, without the container |
| `interop` | the interop scenarios against the Python `iso15118` SECC and `pyslac` | the partners are installed into virtual environments below `target/venv` (`ISO15118_PYTHON_VENV`, `PYSLAC_VENV`) |

The schemas and the EXIficient jars are downloaded in the run (`.github/actions/prepare`), and the vectors made once are cached (`data/`). When a scenario fails, the logs, settings and captures of the runs are uploaded as an artifact (`scenario-artifacts`, `interop-artifacts`; open the `.pcap` with Wireshark). `.github/workflows/audit.yml` checks `Cargo.lock` against the RustSec database weekly, and Dependabot proposes updates of the crates and the actions.

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
- **`cargo xtask gen` changed files under `crates/codec`**: the generator or a schema changed; review the diff and commit it. A different `rustfmt` version can reformat the generated files; use the toolchain CI uses (stable).
- **`cargo xtask linux-test` cannot reach Docker**: start your Docker VM (`colima start` or Docker Desktop). With Colima the repository must be below your home directory, which it shares by default.
- **Namespace tests fail with "Operation not permitted"**: the container is not privileged; use `cargo xtask linux-test` or the dev container as configured.
- **"Permission denied" writing in the repository from the container (Linux host)**: the image's `developer` has UID 1000; the dev container keeps it (`updateRemoteUserUID` is off, so that it and `cargo xtask linux-test` share the build volumes), and so does `cargo xtask linux-test`. If your UID is not 1000, run `docker run --user $(id -u) ...` yourself or use the dev container.
- **Build output owned by root after an older image**: the container runs `own-volumes` (a `chown` of the two volumes) on start, in the dev container and in `cargo xtask linux-test`.
- **Tests fail in the container with a path like `/work/...` or `Permission denied` from `cargo xtask`**: the build volume was made by an earlier layout with the repository at another path. Remove the volumes (`docker volume rm evcc-rs-target-v2 evcc-rs-cargo-v2` with no container using them) and rebuild.
- **Leftover namespaces after an aborted run**: `ip netns list` inside the container shows them; they vanish when the container ends.
