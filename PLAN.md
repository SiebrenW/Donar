# EVCC

The intent of this project is to have an open source implementation of an EVCC using HLC via the standards `ISO 15118-2`, `ISO 15118-20`, `IEC 61851-23` and `DIN Spec 70121`.

Unlike JosEV, it will be portable enough to deploy on resource constrained, realtime, systems (so it can work with nostd). It can also be used as a test platform for EVSE software.

## Scope

Milestone 1 targets **ISO 15118-20**, since it dictates the abstraction for the EV side.

- DC charging, no BPT, Dynamic control mode.
- Authorization: **EIM only**. PnC is a later milestone, added behind the `Ev` trait (certificate and signing hooks).
- Transport: both plain TCP and TLS.
- SLAC comes later. It is tested on Linux, using `unshare` network namespaces to separate the EV from the EVSE simulation.
- Later milestones (BPT, PnC, AC, ISO 15118-2, DIN SPEC 70121) are evaluated when we get there.

## Architecture

### Sans-IO core

The core is a pure `no_std` state machine. It is driven by events (received bytes, timer expiry, EV input) and emits actions (bytes to send, arm/cancel timer, EV requests). Because it asks the host to arm timers, it never needs to be polled. Host adapters supply the real I/O.

- **V2GTP framing and SDP live in the core.** SDP is a separate small sans-IO machine from the session machine. The host runs SDP, gets the result (address, port, secure or not), opens the connection, then starts the session machine.
- **TLS lives outside the core**, in the host adapter (the `openssl` crate on Linux, something else on embedded). The core only sees a byte stream and a "secured" flag.

### EV side: hybrid

- An `Ev` trait for static and instant facts (EVCCID, battery limits, supported services), so simulated and real EVs are easy to swap.
- Events and actions for anything physical or slow (cable check, precharge, contactors, dynamic-mode targets, stop intent).

The exact trait contents are settled slice by slice.

### Memory: no-alloc, arena-based

- The core and codec are `no_std` with no `alloc`. An optional `alloc` feature only adds convenience helpers.
- The worst-case message is large; keeping it inline overflows the stack. So:
  - **Transient messages** are decoded into and encoded from a bump arena. They borrow from it (`<'a>` lifetimes) and are only valid for one step. The arena is reset between messages.
  - **Persistent session state** (negotiated parameters, selected service, schedule) uses small inline fixed-size types. Anything needed across steps is copied out of the message.
- **The host provides the memory.** Separate RX, TX and arena buffers are passed in as `&mut [u8]`, so the host decides placement and sizes (static, heap, or a specific RAM region).
- Running out of arena or buffer space is a defined error (`ArenaFull`, `BufferTooSmall`, `CapacityExceeded`), never a panic or silent truncation.

### Logging

- The core has no logging dependency. It emits **typed log records** through a sink trait (message RX/TX, state changes, timers, response codes, decode errors).
- Adapters render them: `log`/`tracing` on Linux, `defmt` or a ring buffer on embedded, or nothing. Tests can assert on the records directly.
- Levels by meaning: per-session milestones and non-OK response codes at `info`; per-message events and timers at `debug`; raw payload hex and decoded contents at `trace`.
- The `DC_ChargeLoop` repeats for the whole session, so at `debug` and above only changes are logged (a target changed, a limit hit, a non-OK code). Full loop contents are `trace`.
- Errors carry context: message kind, state, and field or response code.
- A Cargo feature can strip `trace` at compile time.

## Codec generation

ISO 15118-20 messages are schema-informed EXI. A generator tool (`exi-gen`) reads the XSDs and emits `no_std`, no-alloc, arena-based Rust.

- The XSDs are freely downloadable but ISO-copyrighted, so they are **gitignored** and fetched by a download script.
- The generated code is also **gitignored** (it is a transcription of the schema). A fresh clone runs `cargo xtask setup` to download the XSDs and generate the codecs.
- The generator must be **fast**: hash-cached and incremental, skipping work when the XSDs have not changed.
- Where the XSD declares `maxOccurs` / `maxLength` they are used as validation limits. Where it is unbounded, the limit is a named constant, recorded as a decision rather than guessed.

### Verifying the codec

A generator that only round-trips its own output can be consistently wrong, so:

1. **Independent reference vectors** are the correctness oracle: `(XML, EXI bytes)` pairs from an independent EXI implementation (e.g. EXIficient) run against the same XSDs and options. Our codec must match byte-for-byte in both directions. The generation script lives in `xtask` (needs Java, optional, cached). The vectors are gitignored like the XSDs.
2. Round-trip and property tests as an ongoing safety net.
3. Fuzzing the decoder, so a hostile EVSE can never cause a panic or overrun the arena.
4. Captured traffic and interop against an external EVSE stack, later, in the Linux harness.

## Layout

Cargo workspace:

```
codecs/
  exi/                      # no_std runtime shared by all codecs: bit I/O, arena, string table, event-code layout
  app-protocol/             # supportedAppProtocol handshake; used before a standard is chosen, so it is its own crate
    src/generated/          # gitignored, from V2G_CI_AppProtocol.xsd
  iso15118-20/              # package iso15118-20, lib iso15118_20
    src/generated/          # gitignored: common_types.rs (header, RationalNumber, response codes), common.rs, dc.rs
    # later: ac, acdp, wpt (Cargo features), bpt
  # later: din70121/, iso15118-2/ (generated the same way, from their schemas)
exi-gen/                    # host tool (lib + bin): XSD -> Rust codec source; `dump` prints the derived grammar
evcc-core/                  # no_std sans-IO EV session + SDP machines, Ev trait, actions/events, log records
evse-sim/                   # in-repo EVSE simulator, the mirror image of the core
evcc-linux/                 # host adapter: sockets, OpenSSL, timers, later SLAC
xtask/                      # setup: XSD download, generation, reference vectors
```

Naming: directories and packages are kebab-case, files and modules snake_case, types PascalCase. Standard names are not repeated in file names; the crate and module path carry them (`iso15118_20::common::decode`).

## Testing

1. **In-repo EVSE simulator** (`evse-sim`, Rust, reuses the generated codec): fast, deterministic, scriptable with fault injection, runs on any dev machine without PLC hardware.
2. **External interop suite** against an independent EVSE stack, run in the Linux `unshare` harness once SLAC exists. This guards against us and the simulator misreading the spec the same way.

## Build order

Built as **vertical slices**: first get `SupportedAppProtocol` and `SessionSetup` working end to end (generator, codec, session machine, simulator, loopback TCP), then add message pairs a few at a time. Every slice runs and is tested, so generator, arena, action/event and `Ev` trait design problems surface early.

0. Workspace, `xtask setup` (download, mtime-cached generation).
1. `exi-gen` and the `iso15118-20` codec for `common` and `dc`, with reference vectors. Done; the generated codecs replaced the hand-written ones.
2. V2GTP, then the SDP machines.
3. `evcc-core` session machine, covering the first full DC dynamic EIM flow:
   - `SupportedAppProtocol` -> `SessionSetup`
   - `AuthorizationSetup` -> `Authorization`
   - `ServiceDiscovery` -> `ServiceDetail` -> `ServiceSelection`
   - `DC_ChargeParameterDiscovery` -> `ScheduleExchange`
   - `DC_CableCheck` -> `DC_PreCharge` -> `PowerDelivery`
   - `DC_ChargeLoop` -> stop -> `SessionStop`
4. `evse-sim` as the peer, with an end-to-end run over loopback TCP.
5. `evcc-linux` with real sockets and TLS.
6. SLAC and the `unshare` harness.
7. External EVSE interop.
8. Later milestones: BPT, PnC, AC, then ISO 15118-2 and DIN SPEC 70121.

Steps 1 to 4 are interleaved per slice rather than finished in sequence.

## Open, to settle slice by slice

- The contents of the `Ev` trait.
- Error and timeout policy (the V2G sequence and message timeouts are central to -20).
- The XSD download source and the exact set of schemas.

## Decided during implementation

- **EXI options** (verified against EXIficient 1.0.7): schema-informed, bit-packed, default (non-strict) fidelity, no options in the header, so streams start `80 00`. Strict mode produces different streams (`80 07`) and is not what real stacks use.
- **Schema source:** `https://standards.iso.org/iso/15118/-20/ed-1/en/` (same as cbExiGen's auto-download).
- **Reference vectors:** `xtask/java/EncodeVector.java` plus the EXIficient jars (downloaded into the gitignored `vectors/lib`). Inputs live in `xtask/vectors/*.xml`. Needs a JDK; on this Mac Homebrew's `openjdk` is at `/opt/homebrew/opt/openjdk/bin`.
- The AppProtocol XSD has no `elementFormDefault="qualified"`, so its child elements are in no namespace; only the root carries `urn:iso:15118:2:2010:AppProtocol`.
- **Event codes** (default fidelity, verified): a state with one production plus the escape takes 1 bit; the document root and repeat states take 2; every typed value has a 1-bit CH before it and a 1-bit EE after. Finite `maxOccurs` is **unrolled** (after the 20th `AppProtocol` only EE remains, coded in 1 bit). The generator must reproduce this.
- **String table:** a repeated string value is a hit (`uint 0`, then ⌈log2(table size)⌉ bits); the table grows only on misses. Needed in both directions.
- **Strings** are a length+2 unsigned int followed by one unsigned int per code point; `unsignedInt` is an EXI unsigned int, `unsignedByte` 8 bits, a bounded int is offset-from-minimum in ⌈log2(range)⌉ bits, enums are ⌈log2(n)⌉ bits.
- **Root event code** (verified): the document grammar holds every global element of the schema *and its imports* (CommonMessages 28 + CommonTypes 2 + xmldsig 24 = 54), sorted by local name across namespaces, plus `SE(*)`. The root code is the index in that list, in ⌈log2(N+1)⌉ bits (6 for `common`: `SessionSetupReq` = 35, `Res` = 36). `xtask/java/DumpRoots.java` and `DumpGrammar.java` print EXIficient's view for cross-checking.
- **Enumerations** are the declaration-order index in ⌈log2(n)⌉ bits (`responseCodeType`: 40 values, 6 bits). `xs:unsignedLong` is an EXI unsigned int (up to 10 octets); `xs:hexBinary` is a length uint plus raw bytes.
- **Open: empty string elements.** EXIficient codes `<EVSEID></EVSEID>` with event code `1` where a non-empty string has CH `0`; the first-level grammar only lists `CH[STRING]`, so it is either a second-level escape (with a second part of unknown width) or an extra EE. The `session_setup_res_empty` vector captures it; the codec rejects empty strings explicitly until resolved. A probe that disambiguates: `SessionStopReq` with an empty `EVTerminationCode` followed by `EVTerminationExplanation`.
- **V2GTP / SDP constants** (checked against ISO 15118-20:2022): header `01 FE` (Tables 12/13), 16-bit payload type (Table 14), 32-bit length. SDP uses multicast `FF02::1`, UDP port 15118; request payload = security (`00` TLS, `10` none) + transport (`00` TCP) (Table 21, V2G20-141/623); response = 16-byte address, 16-bit port, security, transport (V2G20-153). EVCC timing: wait at least 250 ms (V2G20-159), then repeat (V2G20-160), at most 50 requests (V2G20-161), then a timeout (V2G20-162). -20 only runs over TCP+TLS (V2G20-1804/1805); plain TCP here is a test mode.
- **-20 payload types** double as grammar selectors: `0x8001` SAP (AppProtocol schema), `0x8002` mainstream (CommonMessages), `0x8003` AC, `0x8004` DC, `0x8005` ACDP, `0x8006` WPT. The session machine picks the codec module from the payload type.
- **Crate layout change:** V2GTP framing and the SDP message types live in a small shared `v2gtp` crate (the EVSE simulator needs them too); only the SDP *client machine* is in `evcc-core`.
- **Session machine shape** (`evcc-core::session`): host reads from the socket straight into `Session::rx_space()` (host-provided RX buffer) and reports `Event::Received(n)`; V2GTP reassembly happens in the core. Each call returns a `Step { send, timer, notice, end }`; `send` is a fully framed message in the host-provided TX buffer. Decoded messages live in the per-frame arena; only small fixed state (session id) persists. Typed `log::Record`s go to a `LogSink` and may borrow from the message.
- **Timeouts** (Table 215): `V2G_EVCC_Msg_Timeout` is 2 s for every request except `ServiceDetailReq` and `CertificateInstallationReq` (5 s); `V2G_EVCC_Ongoing_Timeout` 60 s (responses with `EVSEProcessing` Ongoing); `V2G_EVCC_Sequence_Performance_Time` 40 s; `V2G_SECC_Sequence_Timeout` 60 s (0.5 s for the charge loop responses, to be handled with the loop). The EVCC repeats a request while Ongoing within the sequence performance time (V2G20-1582).
- **`Ev` trait so far**: `evcc_id()`, `now_micros()`, `dc_connector()`, `dc_limits()`, `schedule_request()`.

## Status

Done: workspace + `xtask setup/gen/vectors/grammars`; EXIficient oracle; **`exi-gen`**: the `app-protocol` and `iso15118-20` (`common_types`, `common`, `dc`) codecs are generated from the XSDs, cover every message of those schemas (including PnC types and BPT) and match all vectors byte for byte; `cargo xtask grammars` proves the derived grammar of all 38 messages equals EXIficient's; `v2gtp`; SDP client machine; session machine through EIM authorization (including the V2G20-1582 repeat and an ongoing timeout) service selection (`Config::energy_service` + `parameter_filter`) and `DC_CableCheck` (40 s bound, V2G20-700), `DC_PreCharge` (10 s, V2G20-704; done when |inlet - battery| < 20 V, IEC 61851-23 t9), a host action to close the contactors (`Notice::CloseContactors` / `Event::ContactorsClosed`; the adapter's `run` takes a host callback that may answer a notice with an event) and `PowerDeliveryReq(Start)` with a single-entry EVPowerProfile (simplification); the Dynamic `DC_ChargeLoop` (request every `charge_loop_interval_ms` after each answer, 0.5 s response timeout; the EV samples `Ev::dc_limits`/`schedule_request`/`battery_voltage` for every request; `Notice::ChargeLoop` reports the EVSE side), and the shutdown (`Event::StopRequested` from the host or `EVSENotification = Terminate` from the EVSE -> `Notice::StopEnergyTransfer`, host ramps down and sends `Event::EnergyTransferStopped` (V2G20-1310) -> `PowerDeliveryReq(Stop)` -> `Notice::OpenContactors` + `SessionStopReq(Terminate)` -> `SessionStopRes` -> `Ended(Completed)`); `Ev` gained `battery_voltage`/`inlet_voltage`; repeats while `Ongoing` are paced by `Config::poll_interval_ms` (`Timer::Poll`); `ScheduleExchange` (Dynamic mode: `Ev::schedule_request`, repeated while `Ongoing`, same 60 s bound; the Scheduled answer is refused) and `DC_ChargeParameterDiscovery` (vehicle limits via `Ev::dc_limits`; the adapter stops at the first resting state, `State::is_resting`); `evse-sim` and the `evcc-linux` TCP adapter with end-to-end loopback tests (EV session machine vs the simulated EVSE over real TCP).

Open in the charge loop and shutdown: `DC_WeldingDetection` is not sent, the other `EVSENotification` values (Pause, ScheduleRenegotiation, MeteringConfirmation, ...) are ignored, Standby/Pause (`PowerDeliveryReq`), metering confirmation (side stream `0x8102`), session pause/resume, BPT, Scheduled mode, error handling (V2G20 8.6) and the sequence/performance timers beyond the per-message ones. TLS is in `evcc-linux::tls` (see the TLS slice below). SDP sockets are in `evcc-linux::sdp`. Next: SLAC.

## References to the standard

Where code implements a specific, citable behaviour (timers, mandated message handling, tables), its doc comment carries `Ref: ISO 15118-20:2022 <V2G20-NNNN | Table N | clause>`. Rules:

- The standard text is in `standards/` (gitignored). Only cite what you have checked there; if a requirement id cannot be found the comment says `TODO, not yet identified`.
- Checked so far: Table 14 (payload types), Table 21 (SDP encoding), V2G20-159..162 (SDP timing), V2G20-1804/1805, Table 204 + V2G20-1355 (service ids), Table 207 (DC service parameters), Table 215 (timeouts), V2G20-1582 (repeat AuthorizationReq), V2G20-1534..1538 (header TimeStamp), V2G20-2103/2104 (DepartureTime), Annex A response code matrix.
- The header `TimeStamp` is microseconds in EVSE time and increasing (V2G20-1534, 1537); `Session` derives it from `Ev::now_micros` and the offset learned from `SessionSetupRes` (V2G20-1535, 1536, 1538).

- **Timers**: `Timer::{Sdp, Response, Ongoing}`; a step carries up to two `TimerCmd`s (`Arm{timer, ms}` / `Cancel(timer)`); the host reports expiry as `TimerExpired(timer)`. The `Ongoing` timer bounds a repeated `Ongoing` exchange (60 s, `V2G_EVCC_Ongoing_Timeout`, Table 215) and is armed once, on the first `Ongoing` response.
- **Authorization grammar facts** (verified with EXIficient): `AuthorizationServices` (`maxOccurs=2`) is unrolled (2-bit state after the first entry, 1-bit after the second); `xs:boolean` is 1 bit; the EIM/PnC choice is 2 bits and EIM is an empty element followed by an EE. The choice width is unobservable in the EIM vectors (padding); a PnC vector will confirm it.
- **`Ongoing_WaitingForCustomerInteraction`** does not appear in the 2022 text (only in the schema, presumably from the amendment; `standards/ISO 15118-20 DAM 1.pdf`); it is treated like `Ongoing`. Check the amendment.
- **Services grammar facts** (verified with EXIficient, see `codecs/iso15118-20/src/services.rs`): the CommonMessages root codes continue at ServiceDetailReq=29, Res=30, ServiceDiscoveryReq=31, Res=32, ServiceSelectionReq=33, Res=34; attributes are namespace-qualified in these schemas (`attributeFormDefault="qualified"`), so `Name` carries the `cm` namespace and `Exponent`/`Value` the `ct` one; `Parameter` has a 1-bit `AT(Name)` code and a 3-bit choice code (bool=0, byte=1, short=2, int=3, rational=4, finiteString=5); `xs:byte` is an 8-bit offset, `xs:short`/`xs:int` are EXI integers (sign bit, then magnitude, minus one when negative); `maxOccurs` lists are unrolled (1-bit first SE, 2-bit repeat/end state, 1-bit end after the maximum, verified at 8 and 16).
- **String table**: literals enter the global partition and the local partition of their qname (here `Name` and `finiteString`); hits (local `uint 0` + index, global `uint 1` + index) change nothing. Index widths are ceil(log2(partition size)). The decoder counts first, then builds, so storage is exact; table capacity is 64 distinct strings per message.
- **DC service parameters** (Table 207, checked): `Connector` 1 Core, 2 Extended, 3 Dual2, 4 Dual4 (matched via `Ev::dc_connector`); `ControlMode` 1 Scheduled, 2 Dynamic; `MobilityNeedsMode` 1 EVCC, 2 SECC allowed; `Pricing` 0/1/2. The EV picks the first parameter set with its connector, ControlMode 2, MobilityNeedsMode 1.
- **Vector tooling**: `xtask vectors` skips vectors newer than their XML and retries the Java run, because Xerces fetches a DTD from w3.org while loading the xmldsig schema and w3.org rate-limits.
- **DC grammar** (verified with EXIficient): the DC document root has 49 elements (6-bit code; CPDReq = 15, CPDRes = 16, CableCheck 11/12, ChargeLoop 13/14, PreCharge 17/18, WeldingDetection 19/20). The energy transfer mode is a substitution group coded in 2 bits (BPT = 0, plain DC = 1). `TargetSOC` (0..=100) is a 7-bit offset. Optional trailing elements sit in a 2-bit state. In -20 the flow order is ServiceSelection, `DC_ChargeParameterDiscovery`, then `ScheduleExchange`.

## The generator (`exi-gen`)

- **Pipeline**: `xsd.rs` reads the XSD subset the schemas use and errors on anything else; `model.rs` resolves types to EXI datatypes and flattens each complex type into a list of *slots* (attributes first, sorted by name, then the particles; extension = base first); `emit.rs` writes Rust; `dump.rs` prints the grammar for diffing with EXIficient (`xtask/java/DumpTree.java`, structurally identical states merged). `cargo xtask gen` regenerates when a schema or the xtask binary is newer than the output; `cargo xtask grammars` runs the diff.
- **Layout instead of tables** (`exi::layout`): a type's grammar is its slots (`min`, `max`, number of alternatives); the productions of a state are computed on demand (current slot while more copies are allowed, following slots up to the first mandatory one, then EE if everything left is optional), so `maxOccurs=2048` costs nothing. Event code = index, width = `ceil(log2(n + 1))`; the document root has `ceil(log2(global elements + 1))` bits (the `SE(*)` code is the extra one).
- **Facts the grammar diff confirmed**: attributes come before elements and are sorted; substitution groups (head included, even when abstract) and choices list their alternatives in order, the group sorted by local name; a choice and a substitution group are the same thing to the layout; integers with a range of at most 4096 values are n-bit offsets (this includes `xs:byte`, `xs:unsignedByte`, percent), wider unsigned ones are EXI unsigned integers, wider signed ones EXI integers.
- **Generated API**: one struct per complex type (`Copy`, borrowing from the arena via `&'a [T]` / `&'a str`; a lifetime only where needed), one enum per choice or substitution group (`...Choice`; identical choices share one), `#[repr(u8)]` enums for enumerations (`from_index`, `xml_name`), `Message` + `decode(buf, arena)` per module, `Msg::encode(&self, out)` for every request/response. Names are derived by `names.rs` (word splitting: `EVMaximumChargePower` -> `ev_maximum_charge_power`, `PnC` stays `PnC`). Elements typed from xmldsig (`Signature`, `X509IssuerSerial`) are omitted: decoding them is `Unsupported`; a type that *requires* one (`CertificateInstallationReq` and friends) is not generated at all.
- **Decoding lists**: a dry run over a copy of the stream counts the entries, then the arena slice is allocated at exactly that size. Nested lists make this O(depth x size); the string table is cloned for the dry run (capacity 64 distinct strings per message).
- **Stack use**: the generated messages are small (`size_of` of the largest, `ScheduleExchangeRes`, is 224 bytes; `tests/sizes.rs` keeps them under 2 KiB) because lists live in the arena.
- **Fuzzing**: `tests/vectors.rs` flips bits in valid streams and requires that decoding returns. A real fuzz target (cargo-fuzz) is still to do.
- **Open**: empty string elements (EXIficient's event code for them is not understood; both directions refuse); choice enum names such as `AReqAuthorizationModeChoice` are mechanical (override table if they bother anyone); the wrapper types (`ServiceList { service: &[..] }`) are not flattened; AC, ACDP and WPT are not generated yet (add them to `ISO20.files/modules` and decide the module names).

## Compliance review against ISO 15118-20:2022 (2026-10-15)

The standard text is in `standards/`. Reading the EVCC requirements of clause 8.5/8.6 and 7.7 against what is built shows the happy path is right but several mandatory behaviours are missing or wrong. Ordered by priority.

**Fixed (all six, with tests):**

1. `DC_WeldingDetection` (V2G20-1448, 1573, 1461, 1462): after `PowerDeliveryRes(Stop)` the session asks the host to open the contactors (`Notice::OpenContactors` / `Event::ContactorsOpened`), sends `DC_WeldingDetectionReq(Ongoing)` repeatedly while the voltage at the EV's inlet is still above 20 V, then `Finished`, then `SessionStopReq`. The generic 60 s `Ongoing` bound applies (the standard defines no welding timer).
2. Invalid messages are ignored (V2G20-440): wrong message, wrong payload type or undecodable payload only logs `Record::Ignored`/`DecodeFailed`; the response timer keeps running. Still fatal: bad V2GTP framing, our own arena being full, and valid responses whose content we cannot use.
3. `Timer::Setup`: 20 s from `Start` until `SessionSetupRes` (V2G20-2298, 448).
4. `Config::allow_unsecured` (default false): without TLS the session ends with `EndReason::TlsRequired` instead of offering -20 (V2G20-1235, 1237).
5. `WARNING_PowerToleranceNotConfirmed` is tolerated on `PowerDeliveryRes(Start)` (V2G20-1438). Other WARNING codes still end the session, which the standard allows where it defines no remedy (e.g. V2G20-1583 for `AuthorizationRes`); implement the remedies when PnC arrives.
6. `Timer::Sequence` (`sequence_performance_ms`, 40 s): bounds the host actions closing contactors, ramp-down and opening contactors. The charge loop interval (default 100 ms) stays below the 0.25 s of Table 217. `Timers` now holds three commands per step.

**Decided, not yet built:** `SessionStopReq` on error exits (V2G20-2644 for non-critical stops before `Start`), standby/pause, service renegotiation (V2G20-1476), `MeteringConfirmation` on side stream `0x8102` when the EVSE asks (V2G20-1621), resumed sessions (V2G20-2098), the other `EVSENotification` values.

**TLS (slice 5) is bigger than planned** (7.7): TLS 1.3 with *mutual* authentication must be supported (V2G20-1264) so the EVCC needs a client certificate chain; cipher suites `TLS_AES_256_GCM_SHA384` then `TLS_CHACHA20_POLY1305_SHA256` (Table 6, V2G20-1858); named groups `secp521r1` / `x448` in the order of Table 7. EVSE and EVCC test certificates are needed in the loopback and `unshare` tests.

**Verification gaps:** the grammar of every message equals EXIficient's, but value-level vectors exist only for the messages the session uses; the PnC types, BPT and the scheduled-mode types are generated but untested. The empty-string EXI question is still open.

**TLS library decision (2026-10-15).** rustls cannot do this standard: no provider implements `secp521r1`/`x448` key exchange or Ed448, and it has no CBC or static-ECDH suites that ISO 15118-2 needs (Table 7: `TLS_ECDH_ECDSA_WITH_AES_128_CBC_SHA256`, `TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256`; -20 requires them after the TLS 1.3 suites for -2 compatibility, V2G20-2056..2066). Other Rust stacks (boring, wolfssl crate, mbedtls crate, embedded-tls, graviola, RustCrypto) lack groups or TLS 1.3. Decision: the `openssl` crate in `evcc-linux`. Probed with `s_server`/`s_client` on OpenSSL 3.6.4 and 4.0.3: TLS 1.3 mutual auth with P-521 certs, x448 and secp521r1 groups, Ed448 certs, both Table 6 suites, and TLS 1.2 `ECDHE-ECDSA-AES128-SHA256` all work. `ECDH-ECDSA-AES128-SHA256` (static ECDH) is not available in either, so an SECC claiming -2 conformance (V2G2-602) needs another library or an older OpenSSL; the EVCC needs only one suite (V2G2-603).

**TLS slice done.** `evcc-linux::tls` builds client and server `SslContext`s from PEM credentials: TLS 1.3 only, Table 6 suites, groups `secp521r1:x448`, sigalgs `ecdsa_secp521r1_sha512:ed448`, mutual authentication, chain checked against the trust anchors but no host name check. `run` is generic over `Transport` (`TcpStream` or `SslStream<TcpStream>`); the timers still work as socket read timeouts. `evse_sim::serve` serves any stream. `tests/tls.rs` generates P-521 and Ed448 PKIs and runs a full DC session over TLS, and checks that unknown CAs (either direction) and P-256 certificates are refused. Not done: TLS 1.2 backwards compatibility (V2G20-2054 ff.), certificate profile checks of Annex B, OCSP, key erasure on session end (V2G20-2046 ff., the stream drop frees OpenSSL state but does not prove it).

**SDP slice done.** `evcc-linux::sdp::discover(socket, destination, config)` drives the core's SDP client over a UDP socket (requests every 250 ms, at most 50, V2G20-159..162); `multicast_destination(ifindex)` is `[ff02::1%if]:15118`; `interface_index(name)` wraps `if_nametoindex`; `connect` opens the TCP connection to the announced address with the interface as scope. `evse_sim::answer_discovery` is the charger side. `tests/sdp.rs` runs on the IPv6 loopback with unicast destinations (retries, no response, wrong security, then a full session on the announced port). The real multicast path is only unit-tested (the destination address); it needs the `unshare` harness with veth interfaces.

**Linux test lab.** `cargo xtask linux-test [cargo test args]` runs the tests in a privileged container (`docker/linux-test.Dockerfile`: `rust:1` + iproute2) with the repo mounted and the target dir and registry in named volumes, so it works from macOS through Colima. `evcc-linux/tests/lab/` creates two named network namespaces joined by a veth pair (`fe80::1` EVCC, `fe80::2` SECC, `nodad`) and runs closures in them with `setns` on a thread. `tests/netns.rs` (Linux only) does real multicast SDP to `ff02::1` and a full DC session across the pair. The whole workspace passes in the container.

**Verdict:** on track. With items 1 to 6 fixed, the message flow of milestone 1 follows the standard; TLS, the SDP socket loop and SLAC remain.
