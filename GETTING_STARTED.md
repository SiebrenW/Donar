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
2. The first start builds the image from `docker/linux-test.Dockerfile` (Rust, JDK, iproute2) and then runs `cargo xtask setup && cargo xtask vectors`. That downloads the ISO 15118-20 schemas, generates the codecs and builds the reference vectors. It takes a few minutes and needs network access.
3. In a terminal inside the container:

   ```sh
   cargo test --workspace
   ```

Zed ignores the `customizations.vscode` block (extensions, settings) of `devcontainer.json` and brings its own rust-analyzer. The setup has been tried with VS Code's settings format and the plain Docker commands; the Zed route is untested, in particular whether Zed passes `--privileged` from `runArgs`. If the namespace tests fail with "Operation not permitted" in a Zed container, run them from a normal terminal with `cargo xtask linux-test`.

You work as the user `developer` (UID 1000) with passwordless `sudo`. Only the test binaries run as root: the image sets `CARGO_TARGET_*_RUNNER` to `docker/test-runner.sh`, which starts programs from cargo's `deps` directory through `sudo`, so the namespace and raw-socket tests get their privileges while building and `cargo xtask` stay unprivileged. The container is **privileged** (the namespace tests need it), so use it on a machine and Docker VM you trust. Build output lives in the named volume `evcc-rs-target` (`CARGO_TARGET_DIR=/target`), so it never mixes with a `target/` directory made on macOS.

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
cargo test -p evcc-linux --test tls       # one test file
cargo xtask linux-test                    # the whole workspace inside the Linux container
cargo xtask linux-test -p evcc-linux --test netns   # only the namespace tests
```

`cargo xtask linux-test [cargo test arguments]` builds the image, starts a privileged container with the repository mounted and runs `cargo test` in it. It is what you use from macOS without the dev container. In the dev container you run `cargo test` directly.

### What the tests cover

| Crate / file | What it checks | Needs Linux root |
|---|---|---|
| `codecs/exi`, `codecs/app-protocol`, `codecs/iso15118-20` | The generated EXI codec byte for byte against EXIficient vectors, in both directions; truncated and bit-flipped input; message sizes | no |
| `v2gtp`, `homeplug` | V2GTP and SDP framing; SLAC message wire formats | no |
| `evcc-core/tests/session_flow.rs` | The session machine message by message: setup, authorization, services, charge parameters, schedule exchange, cable check, pre-charge, power delivery, charge loop, welding detection, session stop, timeouts and the ignore policy | no |
| `evcc-core/tests/slac_flow.rs` | The SLAC state machine against the simulated charger in virtual time: retries, thresholds, several chargers, lost messages | no |
| `evcc-linux/tests/loopback.rs` | The session over real TCP on localhost against the simulator | no |
| `evcc-linux/tests/tls.rs` | TLS 1.3 with mutual authentication, P-521 and Ed448 certificates, refusal of unknown CAs and wrong curves | no |
| `evcc-linux/tests/sdp.rs` | SDP discovery over UDP on the IPv6 loopback | no |
| `evcc-linux/tests/netns.rs` | Two network namespaces joined by a veth pair: multicast SDP, SLAC on a raw packet socket, and the whole bring-up (SLAC, SDP, TLS, DC session) | **yes** (Linux only) |

Tests that need root are compiled only on Linux, so `cargo test --workspace` on macOS simply does not contain them.

## Layout

```
codecs/exi             shared EXI runtime (no_std)
codecs/app-protocol    SupportedAppProtocol messages (generated)
codecs/iso15118-20     ISO 15118-20 messages (generated)
exi-gen                the code generator (XSD to Rust)
v2gtp, homeplug        V2GTP/SDP framing, SLAC messages (no_std)
evcc-core              the sans-IO session, SDP and SLAC machines (no_std)
evse-sim               a simulated charger for tests
evcc-linux             host adapter: TCP, TLS (OpenSSL), UDP/SDP, raw sockets (SLAC)
xtask                  `cargo xtask ...` automation
docker/, .devcontainer the Linux test environment
```

## Troubleshooting

- **`missing .../vectors/*.hex`**: run `cargo xtask setup && cargo xtask vectors`.
- **OpenSSL not found (macOS)**: `export OPENSSL_DIR=$(brew --prefix openssl@3)`.
- **`cargo xtask linux-test` cannot reach Docker**: start your Docker VM (`colima start` or Docker Desktop). With Colima the repository must be below your home directory, which it shares by default.
- **Namespace tests fail with "Operation not permitted"**: the container is not privileged; use `cargo xtask linux-test` or the dev container as configured.
- **"Permission denied" writing in the repository from the container (Linux host)**: the image's `developer` has UID 1000; the dev container adjusts it to your UID (`updateRemoteUserUID`), `cargo xtask linux-test` does not. If your UID is not 1000, run `docker run --user $(id -u) ...` yourself or use the dev container. On macOS with Colima or Docker Desktop this does not occur.
- **Build output owned by root after an older image**: the container runs `own-volumes` (a `chown` of the two volumes) on start, in the dev container and in `cargo xtask linux-test`.
- **Leftover namespaces after an aborted run**: `ip netns list` inside the container shows them; they vanish when the container ends.
