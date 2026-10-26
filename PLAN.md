# Donar

Design decisions and open work. What is finished is in the git history (`git log`) and in the tests; this file keeps what still guides the design, the assumptions that were made without a way to check them, and what is left to do. Contradictions and silences of the standards, with the decision taken, are in [docs/standards-notes.md](docs/standards-notes.md).

Donar (called `evcc-rs` until it grew past the vehicle side) is an open source implementation of the high level communication between vehicle and charger: ISO 15118-20, ISO 15118-2, DIN SPEC 70121 and SLAC, both the vehicle (EVCC) and the charger (EVSE/SECC). The cores are `no_std` and sans-IO, portable to resource constrained, realtime systems, and the simulators make them test platforms for each other and for third-party implementations.

## Where things stand

Implemented and tested (in memory, as scenarios in network namespaces, and against independent partners where one exists):

- **Vehicle** (`evcc-core`): ISO 15118-20 DC and AC, Dynamic and Scheduled, BPT (including the V2X energy range), EIM and Plug & Charge, mobility needs by the charger, `EVEnergyOffer`, schedule and price evaluation, standby, pause and resume, `GoToPause`, service and schedule renegotiation, metering confirmation, welding detection with the welded-contactor outcome; ISO 15118-2 DC and AC (PnC, pause, renegotiation, value-added service details, metering receipt, sales tariffs); DIN SPEC 70121 DC; SDP; SLAC (attenuation correction per carrier group, validation by control pilot).
- **Charger** (`evse-core`): the same three protocols behind the `Secc`, `Iso2Secc` and `DinSecc` traits.
- **Hosts** (`evcc-linux`): sockets, OpenSSL TLS (1.3 mutual authentication, 1.2 for -2), certificate profile and OCSP checks, message signatures, QCA7000 family modem driver.
- **Control pilot**: the host watches the pilot and reacts to an emergency itself; the machines publish what to watch (`Step::pilot`, a `Guard` that is armed and disarmed as the session goes) and take every reading with `Event::Pilot` (the `control-pilot` crate has the rules); emergencies end the session at once without a word, the charger checks C/D and B at the cable check and the stop; vehicle faults of its own before the power (nothing compatible offered) are announced with `SessionStopReq`; the simulators share a pilot "wire".
- **Tooling**: a `capture.pcap` of the cable per scenario run, with the TLS secrets disclosed for dsV2Gshark; generated EXI codecs checked against EXIficient, simulators with fault injection, a sandbox of network namespaces and a scenario runner, interop against the Python `iso15118` SECC and `pyslac`, and a web UI with browser tests.

Independent checks exist against the Python `iso15118` library (-20 DC, AC, BPT, PnC, mobility needs; -2; DIN; the vehicle side only) and `pyslac` (SLAC, vehicle side), and on a Heliox board with a QCA7005 modem (frames, key, network id).

## Open work

### Protocol, vehicle

- A zero first power schedule entry without `GoToPause`: the vehicle would start at zero.
- Scheduled control mode in DC (only the Dynamic loop exists) and Scheduled BPT; pause of a Scheduled session by the vehicle's own wish.
- Reactive power setpoints (sent as 0), the BPT service parameter `GridCodeIslandingDetectionMethod`, the target frequency (V2G20-1555), separated channel and grid forming BPT.
- ACDP and WPT: the codecs exist (`iso15118_20::acdp`, `wpt`, checked against EXIficient). ACDP (infrastructure pantograph for buses, service `DC_ACDP`/`DC_ACDP_BPT`, ID 4 and 7) is done in three steps; **steps 1 and 2 are done** (step 1: the wireless SDP (`v2gtp::sdp::Wireless*`, the vehicle client with its `Ongoing` rule, `evse-sim` with a PPD that reads the EVID from the shared world) and the positioning loop (`ACDP_VehiclePositioning` in both cores, three cases of positioning support, the loss of association; `Ev::evid`, `acdp_positioning_support`, `immobilized`; `Notice::AcdpPosition`; scenarios `acdp-positioning-rfid`, `-optical`, `-unguided`, `acdp-leaves-the-zone`; step 2: the services `DC_ACDP`/`DC_ACDP_BPT` in both cores and the simulators (Tables 210, 211: parameter sets without `Connector`), `ACDP_Connect` after the schedule exchange and `ACDP_Disconnect` in place of the welding detection (`Notice::AcdpConnection`, `Ev::acdp_electrical_status`, `Secc::acdp_connect/acdp_disconnect`), the pantograph model of `evse-sim` with its movement time and the faults `stuck_lowering` and `stuck_raising`, scenarios `acdp-dc`, `acdp-dc-bpt`, `acdp-pantograph-stuck-down`, `acdp-pantograph-stuck-up`). Step 3a, the status stream, is done too (`ACDP_SystemStatus` on payload type 0x8103: `Timer::Status`, `Event::AcdpStatusChanged`, `Notice::AcdpSystemStatus`, `Ev::acdp_technical_status`, `Secc::acdp_system_status`; the simulated radio's strength follows the approach script; scenario `acdp-wlan-fades`). Step 3b is done as well: the pilot through the contacts (`Wire::through_contacts`), the emergencies and the error handling of V2G20-4025 to 4044 (`acdp_wind_down`; the charger keeps the connection: `Secc::acdp_emergency`, `Session::emergency_shutdown`), scenarios `acdp-pilot-emergency-by-evcc/-evse`, `acdp-cable-check-failed`. Not done: the 60 s `System_Status_EndTimer`. Formerly listed for step 2, kept here as the record of what is not done: `ACDP_Connect`/`ACDP_Disconnect` around the DC sequence, the `ACDP_SystemStatus` side stream (0x8103) from `SessionSetupRes` to `SessionStopRes`, the pilot rules (CP B for the vehicle's emergency, E for the charger's) and the error handling that keeps the session until the pantograph is home (V2G20-4025 to 4044, 4057 to 4066). Step 3 holds the status stream, the `wlan` signal strength (`EVWLANStrength`) and the pilot that exists only while the pantograph is down to step 3. Several chargers on one cable (pairing the vehicle with the right bay) is not modelled. WPT (service ID 3): pairing, fine positioning and alignment check before the charge loop; no state machine yet.
- Hash binding of the certificates on a resumed session (V2G20-2539, 2545), the schedule tuple elapsed-time rules (V2G20-1840, 1841).
- Control pilot outputs: the -20 vehicle asks the host for C and B in DC only and only with `Config::control_pilot` (off by default so that hosts that set the pilot themselves are unaffected); DIN and ISO 15118-2 DC always ask, ISO 15118-2 AC asks for C with the Start request and B with the Stop request (V2G2-847, 848). In AC the vehicle holds the first `ChargeParameterDiscoveryReq` until the host has reported 5 % (3 to 7 %) or, in ISO 15118-2, a nominal duty cycle (10 to 95 %); in -20 the static 100 % counts too (V2G20-930 NOTE 1). A host that reports no pilot at all is not held up (`State::AwaitDutyCycle`, ended by the sequence timer).
- Plug & Charge: contract revocation, `SupportedProviders` filtering, a second contract chain, other remedies of V2G20-1583. Certificate installation and update are left out on purpose (messages are in the codecs).
- DIN: stopping between `PowerDeliveryRes(TRUE)` and the first `CurrentDemandReq` with that request at 0 A (V2G-DC-424, 673: "e.g."); the EVSE id is not parsed.
- ISO 15118-2: certificate installation and update; the consumption cost (`RelativePricePercentage`), description and `NumEPriceLevels` of a sales tariff.

### Protocol, charger

- `evse-core` -20: value-added services; an end-to-end test of a failing metering signature.
- The 60 s `V2G_SECC_Sequence_Timeout` ends a session with `Ended::SequenceTimeout`; for DIN the host then carries out the EVSE-initiated emergency shutdown (V2G-DC-366: oscillator off, current down). The simulators do not run the timer.
- The charger answers the cable check and the stop checks of the pilot at once with what it last measured, and `FAILED` when C/D or B is missing: ISO/PAS 15118-23 (`TC23_SECC_CableCheck_005`, `TC23_SECC_WeldingDetectionOrSessionStop_001`) has the charger fail a request that comes without the signalled state, in time, and expects the pilot to be right when the V2G message arrives. So the vehicle is what waits: it sends the cable check and the welding detection only after it measured C/D or B itself (`Event::Pilot`). The simulators read the cable before each request.
- The charger's own emergencies (oscillator off, current down, contactors open) are the host's; the core only learns of the vehicle's. The simulators are hosts that poll every 2 ms instead of arming a monitor; no example of an interrupt driven monitor yet.
- Charger simulator: acting on an `EVEnergyOffer` (it only logs it), an offer with several rules per stack, live mobility-needs changes in the loop, a cable voltage drop (note 2 of V2G20-2185, deleted by Amd 1), the early-stop and pause paths of -2, certificate revocation for the contract.

### Vehicle simulator

- A symmetric-only three-phase vehicle (`Ev::ac20_limits` with `l2`/`l3` `None` on a three-phase connector).
- A constant-voltage taper in the battery model.
- Reactive power; `EVPowerProfile` in Dynamic mode has one entry.

### SLAC and the modem

- Vehicle SLAC and the pilot: the gate, the abort on A/E/F and the restart are done (`evcc_core::slac`). Basic charging after `Outcome::BasicCharging` is done in the simulators only (`evcc-sim` asks for energy with C for `basic_charge_ms` and stops with B; `evse-sim` with `nominal_duty_percent` offers the duty cycle, answers neither SLAC nor SDP and closes its contactor on C; scenario `basic-charging`; `Pwm::offered_current_tenths` is IEC 61851-1 Table A.7). The library has no basic charging mode of its own for a real vehicle (the host decides; the current limit is the offer). Open: the pilot driver on real hardware (`ControlPilot`/`PilotSource` are tested against the simulated wire only), whether the 3 to 7 % window is also right for the later 5 % checks of the session (it is the same `Pwm::is_five_percent`).

- Secure SLAC (SECURITY_TYPE 1); the charger-side timers `TT_EVSE_*`; the charger side of validation (only the EV that started matching, one at a time, V2G3-A09-78) is minimal; `TP_EV_vald_toggle` below 600 ms is not rejected; a real control pilot behind the host trait.
- The QCA modem transmit level (the other half of the attenuation correction).
- QCA modem, unchecked on hardware: the link check by `VS_PL_LNK_STATUS` (tried first, `VS_NW_INFO` second) and the `VS_SET_KEY` alternative (`KeyMessage::Vendor`) follow the QCA7000 documentation as other stacks use it, and are tested against a fake modem only; the 4 s spacing between two keys is from the behaviour other stacks guard against. The modem's `VS_HST_ACTION.IND` is answered whenever the driver reads from the modem, and `Modem::link_lost` reports a link that stayed down for 500 ms (by indication or link status, polled once a second); both are tested against a fake modem only. Not done: no host loop calls `link_lost` yet (the session and the SLAC loops do not watch the modem, a lost link shows as the TCP timeouts), the indication that the chip rebooted (`HAR` 7) is only acknowledged, other chips (Vertexcom MSE102x: `CM_SET_KEY` and `VS_GET_NW_INFO`; Lumissil IS32CG5317: `NSCM_*` messages behind a 19-octet header), a charger-side modem driver (the charger's SLAC in the simulator has no modem; the machine is `evse_core::slac`, mapped in `docs/conformance-15118-3.md`, and `evse-sim` runs it; the CCo role of the QCA driver (`as_coordinator`) and the simulator's `slac_modem` setting are tested against a fake modem only; open: something in the simulator that raises `D-LINK_ERROR` (the sequence after it is carried out, `evse_sim::sequence`), the minimum time at X1 (`x1_ms`, 3 s, from IEC 61851-23, not checked), a charger policy that changes to a nominal duty cycle).
- Hardware: a link between two modems; the `VS_NW_INFO` answer of a modem that joined another network (needs a second modem; the decoder is tested against a frame built from the layout); several networks or stations in that answer; the NID octet layout beyond what the board showed.
- pyslac: `CM_VALIDATE`, link status (`CM_SET_KEY`, the modem side), repeated matching after a failure, a real chip instead of the driver's simulated one.

### TLS and certificates

- Watch [purecrypto](https://docs.rs/purecrypto) (pure Rust, `no_std`, sans-I/O TLS) as a replacement for OpenSSL once it covers ISO 15118-2 TLS. Checked at 0.10.1: no x448 group (V2G20-1637), no `ECDHE_ECDSA_AES_128_CBC_SHA256` or static ECDH suites (V2G2-602), OCSP stapling for the leaf only, no Concat KDF; it has TLS 1.3 with the -20 suites, secp521r1, Ed448, a key log hook and X.509/OCSP. Then: `evcc-linux::tls` behind a trait and the scenarios on it.
- The static-ECDH suite of ISO 15118-2 Table 7 (not in OpenSSL 3.6/4.0): an SECC claiming -2 conformance needs another library; the EVCC does not (V2G2-603).
- Key erasure at the end of a session (V2G20-2046 ff.), cross certificates (V2G20-1002, 1005, 1853), the profiles of the other services (provisioning, e-mobility, OEM provisioning, private environment, OCSP signer), GeneralizedTime encoding, CRLs.
- OCSP: OpenSSL delivers one stapled response, the leaf's, so `require_all` fails for a chain with a sub-CA unless the host brings more responses.
- Identifiers: EMAID and EVSEID follow ISO 15118-2 Annex H and the global identifier of IEC 63119-2; the IDACS format that IEC 63119-2 names for Europe is not at hand and not checked. The EVSEID checks run on the simulators' defaults only, not on what a peer sends.
- ISO 15118-20 Amd 1: what is done and what is not (AC DER services (codecs done), MCS, the OCSP signer chain, cross certificates, private environment, phase element pairing, Table 94 ranges) is in `docs/conformance-15118-20-amd1.md`.

### Codec and generator

- A real fuzz target (cargo-fuzz); today `tests/vectors.rs` flips bits in valid streams.
- Performance left on the table: `Layout::read` (about 9 ns per event), the 24-byte `DecodeError` in every `Result`, straight-line code for plain sequences; code size was never measured.
- Cosmetic: mechanical choice enum names (`AReqAuthorizationModeChoice`), wrapper types (`ServiceList { service: &[..] }`) not flattened.

### Interop and checks against the standard

- Other partners: EVerest (Josev, everest-core), commercial chargers and vehicles, a hardware EVSE. The charger cores have only our own vehicle (and the Python library's EVCC could be a partner).
- No independent verifier for the -20 signature suites (secp521r1/SHA-512, Ed448/SHAKE256); the Python library supports only secp256r1/SHA-256.
- The Python SECC does not offer `BPTChannel` and `GeneratorMode`; whether the standard requires them is unchecked. Discharging was not exercised against it.
- DC renegotiation of -2 is only tested against our own charger (the Python SECC goes back to the cable check, with a TODO in its code).
- Table 224/226 response code applicability was read from the text extraction of the PDF and by meaning.

### Web UI and simulation page

- Control pilot in the page: the Simulation tab shows the cable's reading and the guards of both machines and has controls for the vehicle's state, the charger's oscillator and an emergency of either side; the Scenarios tab has none (a scenario's TOML can: `pilot_emergency_after`, `pilot_emergency_after_loops`).
- Captures: the simulation page does not capture yet (the runs of the Scenarios tab do: the path of `capture.pcap` is in the result); the charger simulator does not disclose in the runner (the vehicle's secrets suffice); a Python-partner TLS run is decryptable only because our vehicle is the client.
- Stopping a run (killing the runner would leave namespaces behind), saving an edited scenario to a file, several runs at once, authentication (localhost only).
- The Scenarios tab keeps its plain form; SLAC and SDP are not phases to hold at, and SLAC in the simulation is untested.
- Fault injection to drop the connection or delay a response.
- A browser test that clicks through a session (the browser tests check that the page builds, every choice can be made and no script errors); forms for PnC.

### CI

- The workflows run on GitHub's hosted runners (Linux jobs pinned to ubuntu-24.04). Failures show up as annotations (`.github/actions/annotate`) because job logs need a login. Not covered: a Windows build (the sandbox is Linux-only), a coverage report, `cargo deny` for licences, a nightly run of the Python partners against their newest versions.
- The code is not formatted with rustfmt (about 2100 differences), so `cargo fmt --check` is not a check.

### Code layout

- `crates/evcc/core/tests/session_flow.rs` could be split per phase like the code; `exi-gen/src/emit.rs` mixes type and codec emission; `evcc-linux/src/lib.rs` carries both the `Transport` trait and the run loop; `evse::on_frame` is one long match.
- The crates, binaries, container image and repository directory still carry the old `evcc-*` names; renaming needs an approved mapping first.

## Assumptions made without a check

The standard is silent or ambiguous, and no independent implementation settles it. They are the first suspects when an interop test fails.

- The digest and the signature cover the EXI fragment *with* its header octet (EXIficient-based stacks do it that way); the SHAKE256 digest is 64 octets; an ECDSA value is `r || s`.
- The schedule renegotiation side stream (`0x8101`) carries the messages coded as on the main stream; the receiver decodes what the exchange expects next. The Python library defines the payload type but does not implement it.
- A negative `EVTargetEnergyRequest` means discharge.
- `TimeAnchor` is microseconds of SECC time (Annex J; Table 112 says milliseconds).
- `GoToPause` is followed by `PowerDeliveryReq(Stop)` (the schema has no "Pause" progress) and `SessionStopReq(Pause)`.
- The MMTYPEs of `CM_VALIDATE` (0x6044/0x6045) are those of the open HomePlug implementations.
- After a wake-up from pause the vehicle goes to the charge parameter discovery (V2G20-1032, 2098), not to a new service selection.
- After an emergency the standards let both sides "try to maintain digital communication" (IEC 61851-23 CC.3.4.3: requests then carry `EVReady = False`, responses `FAILED`). The cores do the stricter thing: the session ends and nothing more is sent or answered.
- A static duty cycle (100 % or oscillator off) in an AC session is the supply stopping (IEC 61851-1 state x1), not an emergency; in DC it is (V2G-DC-656, CC.3.4.3). 5 % is taken as 4 to 6 % of the host's rounded measurement.
- Ed448 SECC certificates carry keyAgreement as Table B.6 says, although an Ed448 key cannot do key agreement.

## Architecture

### Sans-IO core

The core is a pure `no_std` state machine driven by events (received bytes, timer expiry, vehicle input) that returns actions (bytes to send, arm or cancel a timer, requests to the vehicle). It asks the host to arm timers, so it never needs polling. Host adapters supply the I/O.

- V2GTP framing and the SDP machines live in the core; the host runs SDP, opens the connection, then starts the session machine.
- TLS lives outside the core (the `openssl` crate in `evcc-linux`); the core sees a byte stream and a "secured" flag. rustls was ruled out: no provider has `secp521r1`/`x448` key exchange or Ed448, and it lacks the CBC and static-ECDH suites of ISO 15118-2.
- The vehicle describes itself through the `Ev` trait (static facts and present limits); anything physical or slow (cable check, pre-charge, contactors, ramp-down, stop intent) is an event or notice. The charger is the host's implementation of `Secc`/`DinSecc`/`Iso2Secc`; the machines own sequence, session ids, failing responses and state, the host owns the values.
- Crypto stays at the edge: the cores take a `Contract` trait and hand the host the stream a signature covers.
- The vehicle and the charger are split by capability (`Ev`, `DcEv`, `AcEv`, `InvertedPantographEv`, ...; `Secc`, `DcSecc`, ...): one machine drives one connector, so an AC-only vehicle implements no DC and a pantograph vehicle no AC or plug. The cores have additive Cargo features for protocols, energy and connections; `strict` rejects the impossible mixes at compile time; `cargo xtask features` builds a hand-picked list (not the power set). Decided with the user 2026-10-26; the inverted (charger-mounted) pantograph and the roof-mounted (vehicle) one are different variants, only the first is implemented.

### Memory

- `no_std` with no `alloc`; an optional `alloc` feature only adds helpers.
- Transient messages are decoded into and encoded from a bump arena and borrow from it (`<'a>`), valid for one step. Persistent session state uses small inline fixed-size types.
- The host provides separate RX, TX and arena buffers as `&mut [u8]`. Running out is a defined error (`ArenaFull`, `BufferTooSmall`, `CapacityExceeded`), never a panic or silent truncation.

### Logging

- The core emits typed log records through a sink trait; adapters render them (`log`/`tracing` on Linux, `defmt` or a ring buffer on embedded).
- Levels by meaning: milestones and non-OK codes at `info`; messages and timers at `debug`; payload at `trace`. The charge loop logs only changes at `debug`.

### Codec generation and verification

- `exi-gen` reads the XSDs and writes schema-informed, `no_std`, arena-based EXI codecs. The XSDs are gitignored (ISO and DIN copyright) and `cargo xtask setup` fetches them; the generated code is committed, for repeatability and review, and CI regenerates it and fails when it differs. Generation is skipped when the output is newer than the schemas.
- Where the XSD bounds a list or string the bound is the validation limit; where it is unbounded the limit is a named constant.
- EXIficient is the oracle: reference vectors byte for byte in both directions (`cargo xtask vectors`, `cargo xtask grammars` for the derived grammar), plus round-trip and bit-flip tests.
- Facts the codecs depend on (all verified against EXIficient):
  - Schema-informed, bit-packed, default fidelity, no options in the header. `valuePartitionCapacity` is 0 for every protocol (found through the Python partner failing on a repeated string).
  - The document root holds every global element of the schema and its imports, sorted by local name; DIN and ISO 15118-2 use a `V2G_Message` document root. Event code width is `ceil(log2(n + 1))`; finite `maxOccurs` is unrolled; integers with a range of at most 4096 values are n-bit offsets.
  - Empty string elements are coded as the second-level `EE` of the first state; `xs:integer` is of any size.
  - The message signature types are generated into the `xmldsig` crate (the profile of what the standards use; wildcards, `KeyInfo`, `Object`, `XPath`, `HMACOutputLength` and the reference `Type` are not modelled and refused when decoding); the `signature` modules of the standards hold the algorithms and sign EXI *fragments*; the start event is chosen among all element declarations of the schema (-20: the xmldsig schema as entry point for `SignedInfo`, V2G20-1449; -2: two grammars, J.2 and J.4).
- The `-20` payload types double as grammar selectors: `0x8001` SAP, `0x8002` mainstream, `0x8003` AC, `0x8004` DC, `0x8005` ACDP, `0x8006` WPT; `0x8103` is the ACDP system status side stream; `0x8101` schedule renegotiation and `0x8102` metering confirmation are side streams.

### Timing (ISO 15118-20 Table 215 and others)

Message timeout 2 s (5 s for `ServiceDetailReq` and `CertificateInstallationReq`); `Ongoing` 60 s (also the bound for welding detection, for which the standard sets no timer); sequence performance 40 s; SECC sequence timeout 60 s (0.5 s in the charge loop); communication setup 20 s; SDP: at least 250 ms between requests, at most 50. A message that is invalid is ignored and the response timer keeps running (V2G20-440).

### Layout

Cargo workspace, everything under `crates/`; `data/` (downloaded schemas, vectors), `standards/` (your copies of the standards) and `docs/` are as described in AGENT.md and `docs/getting-started.md`.

- `codec/`: wire formats without state machine or I/O (`exi`, `exi-gen`, `app-protocol`, `iso15118-20`, `iso15118-2`, `din70121`, `v2gtp`, `homeplug`).
- `evcc/{core,linux}`, `evse/core`: the cores and the Linux host adapter.
- `sims/`: `evse-sim`, `evcc-sim`, `sandbox`, `scenarios`, `web`, `control`.
- Scenarios are TOML files (parameters, participants, pass criteria); participants run as separate processes in network namespaces; only creating namespaces runs as root, everything else as the invoking user, and no root-owned artifacts are left behind. End-to-end tests are thin wrappers around scenarios; message-level fault tests stay in-process and run anywhere.
- Package names are kebab-case and independent of the directory; files and modules snake_case; standard names are not repeated in file names.
- Cite requirement ids in comments only when checked against the standard text.

## Audit of 2026-10-24

A read of the whole repository for bugs, risks and load (the findings, and what was decided). Fixed: the SDP response is accepted only with an address on the link (link-local or unique local; `accept_other_address` for a test on the loopback), a stapled OCSP response with a time that cannot be read is a violation and no longer a panic, and the generated codecs are committed (CI regenerates and compares) instead of fetched and generated on every clone. Left, because they only concern the test tools (the simulators and the web page), not the product:

- The simulator's network key is a constant (`[0x5A; 16]`, `evse_sim::slac::Fixed`), and with `slac_modem = true` it is set on a real modem. A charger needs a random key for every match (V2G3-A09-92); the machine asks a `Random` for it, and the simulator's is fixed so that tests know it. A real charger host brings its own.
- `Serve::step` sets the key on the modem before it returns the frames, and the modem driver keeps 4 s between two keys; a match that follows a key within seconds (a join failure and an immediate retry) has its confirmation delayed by the sleep.
- The web page has no login and does not look at the `Host` header; the scenario settings carry file paths. It listens on loopback by default and is for tests.
- The simulator's SLAC loop ignores socket errors, and the control socket buffers a line without a limit.
- The charger machine takes `CM_ATTEN_PROFILE.IND` from any source address. SLAC is direct modem to modem; the profile is not secret.
