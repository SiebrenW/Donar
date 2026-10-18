# AGENT.md

Guidelines for Claude Code and other AI agents working on this project. (`CLAUDE.md` only refers here.)

`evcc-rs` is an open-source EV communication controller (the vehicle side, EVCC) for ISO 15118-20, ISO 15118-2 and DIN SPEC 70121, plus SLAC, with a charger side (EVSE/SECC) and simulators to test against. Documents: `README.md` (how to build your own EVCC/EVSE), `docs/getting-started.md` (setup, what the tests cover), `PLAN.md` (design log and status of every requirement), `docs/standards-notes.md` (where the standards contradict themselves or are silent, and the decision taken). Read the relevant ones before larger changes. `docs/` is meant to hold Typst documents in the future; `AGENT.md` and `README.md` stay in the root.

## Commits

- **No AI attribution lines**: Do not add "Co-Authored-By: Claude [Model]" or similar lines to git commit messages. Omit them entirely.
- **Regular commit practice**: Follow standard git practices for the project. Keep commit messages clear and focused on the changes made.

## General

- Follow the project's existing code style and conventions.
- Refer to the main README and PLAN for project architecture and direction.

## Terminology

- **V2G** is the high level communication (ISO 15118, over V2GTP). **BPT** (bidirectional power transfer) or **V2X** is delivering power from the vehicle (discharge). Do not use V2G for discharging (code, comments, UI, docs).

## Commands

A fresh clone does not build the codecs: the schemas, generated code and EXIficient vectors are gitignored (`data/`, `standards/`, `crates/codec/*/src/generated/`).

```sh
cargo xtask setup            # download the schemas, generate the codecs (needs curl; --force regenerates)
cargo xtask vectors          # reference vectors with EXIficient (needs a JDK)
cargo xtask grammars         # diff the derived grammar with EXIficient's
cargo test --workspace       # everything that runs on this machine (the Linux-only tests are cfg'd out on macOS)
cargo test -p evcc-core --test slac_flow          # one test file
cargo test -p evcc-core <name>                    # one test by name
cargo xtask linux-test [cargo test args]          # inside the privileged Linux container (network namespaces, raw sockets)
cargo xtask linux-test -p scenarios --test scenarios dc_tls_slac   # one scenario
cargo xtask interop [name...]   # scenarios in scenarios/interop/ against the Python `iso15118` SECC and `pyslac` (container)
cargo xtask web [PORT]          # the web UI (evcc-web) in the container, default http://127.0.0.1:8080/
```

`cargo xtask` is an alias for the `xtask` crate (`.cargo/config.toml`). In the dev container run `cargo test` directly. The repository must be at `/workspaces/evcc-rs` in the container (cargo bakes `CARGO_MANIFEST_DIR` into outputs on a shared volume). Browser tests (`crates/sims/web/tests/browser.rs`) need Chrome/Chromium/Brave (`CHROME`), are skipped without one unless `EVCC_REQUIRE_BROWSER` is set (the container sets it). macOS `sed` has no `\b`; check renames with a grep afterwards.

## Architecture

**Sans-IO cores.** `crates/evcc/core` (vehicle) and `crates/evse/core` (charger) are `no_std`, allocation-free state machines that never touch a socket, clock or thread. A host feeds `Event`s (bytes received, timer fired, vehicle did something) and carries out the returned `Step` (bytes to send, timer commands, `Notice`s for the vehicle to act on). The host owns the rx/tx/arena buffers. Host adapters: `crates/evcc/linux` (sockets, OpenSSL TLS, raw SLAC socket, QCA modem driver, `run`/`run_gated` loops).

- Vehicle: `evcc_core::session` (ISO 15118-20, split into `setup/parameters/charge/shutdown/renegotiation/metering/...`), `iso2`, `din`, `slac`, `sdp`; all session machines implement `machine::Machine`. The vehicle describes itself through the `Ev` trait (`ev.rs`): capabilities (discovery) and present limits (charge loop) are separate methods.
- Charger: `evse_core::{Session, din::Session, iso2::Session}` behind the traits `Secc`, `DinSecc`, `Iso2Secc` (what to offer, how to answer, faults via `intercept`/`note`). The machines own sequence, session-id checks, failing responses and state; the host owns values. The `SupportedAppProtocol` negotiation of DIN/ISO 15118-2 is the host's (evse-sim does it).

**Codecs are generated.** `crates/codec/exi-gen` reads the XSDs (`data/schemas`) and writes `no_std` schema-informed EXI codecs into `crates/codec/{app-protocol,din70121,iso15118-2,iso15118-20}/src/generated/` (via `cargo xtask gen`; runs `rustfmt`). Never edit generated files; change the generator. The runtime is `crates/codec/exi`. EXIficient is the oracle: `cargo xtask vectors` makes byte-exact reference vectors (`data/vectors/*.hex`, inputs in `crates/xtask/vectors/*.xml` + manifest) that the codec tests round-trip. Message signatures use hand-written `signature` modules (xmldsig is not modelled by the generator) over EXI *fragments*. `homeplug` and `v2gtp` are small hand-written codecs.

**Simulators and test harness** (`crates/sims`): `evcc-sim` and `evse-sim` are ordinary commands with TOML settings (the vehicle/charger cores behind sim `Ev`/`Secc` implementations, with fault injection); `sandbox` makes Linux network namespaces; `scenarios` runs a scenario TOML (`crates/sims/scenarios/scenarios/*.toml`, format documented in `scenario.rs`) as two processes in two namespaces, optionally against an external partner (`Kind::Iso15118Python`, `Kind::PySlac`; partner setup in `partners.rs`, venvs at `/opt/iso15118` and `/opt/pyslac` in the container); `web` is the axum UI (one embedded `src/index.html`, compiled into the binary, so rebuild after editing it); `control` is the live-control socket protocol. Every scenario file needs an entry in `tests/scenarios.rs` (a test checks this).

**Crypto stays at the edge.** Plug & Charge, certificate profile/OCSP checks and signing live in `evcc-linux`/the sims (OpenSSL); the cores take a `Contract` trait for the vehicle's keys and hand the host the stream a signature covers. Certificate installation/update is deliberately out of scope.

## Working conventions

- Cite requirement ids (`V2G20-…`, `V2G-DC-…`) in comments only if checked against the standard text; the source is `standards/` (PDFs, gitignored; extract the text to search it).
- After protocol changes add: a session-flow/against-evcc test, and where behaviour crosses the wire a scenario (plus a Python interop scenario if the partner supports it) and a PLAN.md entry. When the standard contradicts itself or is silent, record the decision in `docs/standards-notes.md`.
- In `docs/standards-notes.md` list only real contradictions, silences and impossible requirements. An element that is optional in the schema but required in some cases by the text is normal use of optional elements, not an inconsistency. Record the decision taken, not the earlier wrong state.
- Scenario `expect.evcc_log` cannot match `MsgTx` lines (Debug level); charger log lines come from the core's `note` calls.
