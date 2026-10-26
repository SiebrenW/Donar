# AGENT.md

Guidelines for Claude Code and other AI agents working on this project. (`CLAUDE.md` only refers here.)

Donar (the project was called `evcc-rs`) is an open-source toolkit for the vehicle-to-charger high level communication: ISO 15118-20, ISO 15118-2, DIN SPEC 70121 and SLAC, with a vehicle side (EVCC), a charger side (EVSE/SECC), generated EXI codecs, and simulators, scenarios and a web UI to test with. It is not only about the EVCC. The crates, binaries, container image and the repository directory still carry the old `evcc-rs`/`evcc-*` names. Documents: `README.md` (how to build your own EVCC/EVSE), `docs/getting-started.md` (setup, what the tests cover), `PLAN.md` (design decisions, unchecked assumptions and the open work; what is done is in git history), `docs/standards-notes.md` (where the standards contradict themselves or are silent, and the decision taken). Read the relevant ones before larger changes. `docs/` is meant to hold Typst documents in the future; `AGENT.md` and `README.md` stay in the root.

## Commits

- **No AI attribution lines**: Do not add "Co-Authored-By: Claude [Model]" or similar lines to git commit messages. Omit them entirely.
- **Regular commit practice**: Follow standard git practices for the project. Keep commit messages clear and focused on the changes made.

## General

- Follow the project's existing code style and conventions.
- Refer to the main README and PLAN for project architecture and direction.

## Terminology

- **V2G** is the high level communication (ISO 15118, over V2GTP). **BPT** (bidirectional power transfer) or **V2X** is delivering power from the vehicle (discharge). Do not use V2G for discharging (code, comments, UI, docs).

## Commands

Linux is the only supported platform. Develop in the dev container (`.devcontainer/`, repository at `/workspaces/evcc-rs`); from a host terminal `cargo xtask lint|linux-test|interop|web` run in the same container image.

The generated codecs are committed (`crates/codec/*/src/generated/`), so a fresh clone builds. The schemas and the EXIficient vectors are gitignored (`data/`, `standards/`): the tests that compare with the vectors need `cargo xtask setup && cargo xtask vectors`, and so does a change to the generator.

```sh
cargo xtask lint             # clippy with -D warnings
cargo xtask features         # the hand-picked feature combinations of evcc-core and evse-core (--target TRIPLE for bare metal)
cargo xtask setup            # download the schemas, regenerate the codecs (needs curl; --force regenerates); commit what changes
cargo xtask vectors          # reference vectors with EXIficient (needs a JDK)
cargo xtask grammars         # diff the derived grammar with EXIficient's
cargo test --workspace       # everything (Linux only; some tests need root)
cargo test -p evcc-core --test slac_flow          # one test file
cargo test -p evcc-core <name>                    # one test by name
cargo xtask linux-test [cargo test args]          # inside the privileged Linux container (network namespaces, raw sockets)
cargo xtask linux-test -p scenarios --test scenarios dc_tls_slac   # one scenario
cargo xtask interop [name...]   # scenarios in scenarios/interop/ against the Python `iso15118` SECC and `pyslac` (container)
cargo xtask web [PORT]          # the web UI (evcc-web) in the container, default http://127.0.0.1:8080/
```

`cargo xtask` is an alias for the `xtask` crate (`.cargo/config.toml`). In the dev container run `cargo test` directly. The repository must be at `/workspaces/evcc-rs` in the container (cargo bakes `CARGO_MANIFEST_DIR` into outputs on a shared volume). Browser tests (`crates/sims/web/tests/browser.rs`) need Chrome/Chromium/Brave (`CHROME`), are skipped without one unless `EVCC_REQUIRE_BROWSER` is set (the container sets it).

## Architecture

**Sans-IO cores.** `crates/evcc/core` (vehicle) and `crates/evse/core` (charger) are `no_std`, allocation-free state machines that never touch a socket, clock or thread. A host feeds `Event`s (bytes received, timer fired, vehicle did something) and carries out the returned `Step` (bytes to send, timer commands, `Notice`s for the vehicle to act on). The host owns the rx/tx/arena buffers. Host adapters: `crates/evcc/linux` (sockets, OpenSSL TLS, raw SLAC socket, QCA modem driver, `run`/`run_gated` loops).

- Vehicle: `evcc_core::session` (ISO 15118-20, split into `setup/parameters/charge/shutdown/renegotiation/metering/...`), `iso2`, `din`, `slac`, `sdp`; all session machines implement `machine::Machine`. The vehicle describes itself through `Ev` (identity, schedule, contract) and capability traits (`DcEv`, `DcPlugEv`, `AcEv`, `AcPlugEv`, `InvertedPantographEv`); each machine asks for the combination it needs (`Iso20Vehicle`, `Iso2Vehicle`, `DinVehicle`, feature-aware) and reaches the capability methods through `Caps`. Capabilities (discovery) and present limits (charge loop) are separate methods.
- Charger: `evse_core::{Session, din::Session, iso2::Session}` behind the traits `Secc`, `DinSecc`, `Iso2Secc` (what to offer, how to answer, faults via `intercept`/`note`; by capability: `Secc` with `DcSecc`, `AcSecc`, `InvertedPantographSecc`, `Iso2Secc` with `Iso2DcSecc`, `Iso2AcSecc`). The machines own sequence, session-id checks, failing responses and state; the host owns values. The `SupportedAppProtocol` negotiation of DIN/ISO 15118-2 is the host's (evse-sim does it).

**SLAC and the pilot.** The vehicle SLAC machine (`evcc_core::slac`) starts only on a 5 % pilot (`Event::Pilot` before/after `Start`), ends in `Outcome::BasicCharging` on a nominal duty cycle and aborts on A/E/F while matching; hosts without a pilot set `Config::pilot_gate = false` (`evcc_linux::slac::run` does). The host implements `evcc_linux::slac::ControlPilot` (readings, guard, toggles).

**Control pilot.** `crates/pilot` (`control-pilot`, `no_std`) holds the readings (state A to F, PWM) and the rules for what a change means to the vehicle (`EvWatch`) and to the charger (`EvseWatch`); the host watches the pilot itself (fast path) with the `Guard` the machines publish in `Step::pilot` (armed and disarmed as the session goes) and reports every change with `Event::Pilot`. An emergency ends the session at once and sends nothing; the host does the hardware part itself. The simulators share a pilot "wire" (`evcc_linux::pilot::Wire`, two files in the artifacts directory) which the scenario runner sets up for `evse-sim` scenarios.

**Codecs are generated.** `crates/codec/exi-gen` reads the XSDs (`data/schemas`) and writes `no_std` schema-informed EXI codecs into `crates/codec/{xmldsig,app-protocol,din70121,iso15118-2,iso15118-20}/src/generated/` (via `cargo xtask gen`; runs `rustfmt`). Never edit generated files; change the generator, run `cargo xtask gen --force` and commit the result (CI regenerates and fails when the committed files differ). The runtime is `crates/codec/exi`. EXIficient is the oracle: `cargo xtask vectors` makes byte-exact reference vectors (`data/vectors/*.hex`, inputs in `crates/xtask/vectors/*.xml` + manifest) that the codec tests round-trip. The codecs of the three standards do not depend on each other; what they share is what the schemas share, the xmldsig types of the message signature (`crates/codec/xmldsig`, generated from the W3C schema with a profile of what is used: `xsd::XMLDSIG_MODELLED` and `XMLDSIG_UNUSED`). Their `signature` modules hold the algorithms and the encoding of the signed EXI *fragments*. `homeplug` and `v2gtp` are small hand-written codecs.

**Simulators and test harness** (`crates/sims`): `evcc-sim` and `evse-sim` are ordinary commands with TOML settings (the vehicle/charger cores behind sim `Ev`/`Secc` implementations, with fault injection); `sandbox` makes Linux network namespaces; `scenarios` runs a scenario TOML (`crates/sims/scenarios/scenarios/*.toml`, format documented in `scenario.rs`) as two processes in two namespaces, optionally against an external partner (`Kind::Iso15118Python`, `Kind::PySlac`; partner setup in `partners.rs`, venvs at `/opt/iso15118` and `/opt/pyslac` in the container); `web` is the axum UI (one embedded `src/index.html`, compiled into the binary, so rebuild after editing it); the pictures of the simulation page (car and buses) are generated into it between the `scenes:begin`/`scenes:end` markers by `crates/sims/web/art/scenes.py`, do not edit them by hand; `control` is the live-control socket protocol. Every scenario file needs an entry in `tests/scenarios.rs` (a test checks this). Each run captures the cable to `capture.pcap` in its artifacts (`tcpdump`, in the container image) and the vehicle sim discloses its TLS secrets as UDP packets for dSPACE V2Gshark; keep that working when touching the runner or the TLS setup.

**Crypto stays at the edge.** Plug & Charge, certificate profile/OCSP checks and signing live in `evcc-linux`/the sims (OpenSSL); the cores take a `Contract` trait for the vehicle's keys and hand the host the stream a signature covers. Certificate installation/update is deliberately out of scope.

## Features

`evcc-core` and `evse-core` have additive features (protocols `iso20`, `iso2`, `din`; energy `ac`, `dc`; connections `plug`, `inverted-pantograph`; `slac`, `acdp`; `strict`). One machine drives one connector (a plug may do CCS and AC, a pantograph is a connector of its own); `strict` makes the impossible mixes a compile error, the workspace (simulators, tests) builds everything without it. A new feature goes into `crates/xtask/src/features.rs` (a check fails otherwise). Code of a capability that is compiled out is reached through `Caps` / `SeccCaps` / `Iso2SeccCaps`, which answer with neutral values; do not call the capability traits from a machine directly.

## Continuous integration

`.github/workflows/ci.yml` (clippy with `-D warnings`, the `no_std` build for a bare-metal target, the whole workspace on Linux as root, the interop scenarios; see `docs/getting-started.md`). Keep clippy clean with `cargo xtask lint`. The Linux job runs the tests natively on the runner, not in the container, so a change that needs a new tool needs it installed in the workflow as well as in `.devcontainer/Dockerfile`.

## Working conventions

- Cite requirement ids (`V2G20-…`, `V2G-DC-…`) in comments only if checked against the standard text; the source is `standards/` (PDFs, gitignored; extract the text to search it).
- After protocol changes add: a session-flow/against-evcc test, and where behaviour crosses the wire a scenario (plus a Python interop scenario if the partner supports it) and update PLAN.md (remove what is now done from the open work, record new assumptions). When the standard contradicts itself or is silent, record the decision in `docs/standards-notes.md`.
- In `docs/standards-notes.md` list only real contradictions, silences and impossible requirements. An element that is optional in the schema but required in some cases by the text is normal use of optional elements, not an inconsistency. Record the decision taken, not the earlier wrong state.
- Scenario `expect.evcc_log` cannot match `MsgTx` lines (Debug level); charger log lines come from the core's `note` calls.
