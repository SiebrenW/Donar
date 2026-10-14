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
- **TLS lives outside the core**, in the host adapter (rustls on Linux, something else on embedded). The core only sees a byte stream and a "secured" flag.

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
  din70121/                 # package din70121, lib din70121
    src/{lib,types,encode,decode}.rs
  iso15118-20/              # package iso15118-20, lib iso15118_20
    src/lib.rs
    src/common/{mod,types,encode,decode}.rs   # from the CommonMessages XSD
    src/dc/{mod,types,encode,decode}.rs       # DC XSD (milestone 1)
    # later: ac/, bpt/, acdp/, wpt/ (Cargo features)
exi-gen/                    # host tool: XSD -> Rust codec source
evcc-core/                  # no_std sans-IO EV session + SDP machines, Ev trait, actions/events, log records
evse-sim/                   # in-repo EVSE simulator, the mirror image of the core
evcc-linux/                 # host adapter: sockets, rustls, timers, later SLAC
xtask/                      # setup: XSD download, generation, reference vectors
```

Naming: directories and packages are kebab-case, files and modules snake_case, types PascalCase. Standard names are not repeated in file names; the crate and module path carry them (`iso15118_20::common::decode`).

## Testing

1. **In-repo EVSE simulator** (`evse-sim`, Rust, reuses the generated codec): fast, deterministic, scriptable with fault injection, runs on any dev machine without PLC hardware.
2. **External interop suite** against an independent EVSE stack, run in the Linux `unshare` harness once SLAC exists. This guards against us and the simulator misreading the spec the same way.

## Build order

Built as **vertical slices**: first get `SupportedAppProtocol` and `SessionSetup` working end to end (generator, codec, session machine, simulator, loopback TCP), then add message pairs a few at a time. Every slice runs and is tested, so generator, arena, action/event and `Ev` trait design problems surface early.

0. Workspace, `xtask setup` (download, hash-cached generation).
1. `exi-gen` and the `iso15118-20` codec for `common` and `dc`, with reference vectors.
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
