# Donar

[![CI](https://github.com/SiebrenW/Donar/actions/workflows/ci.yml/badge.svg)](https://github.com/SiebrenW/Donar/actions/workflows/ci.yml)

Donar (the Germanic god of thunder) is an open-source toolkit for the high level communication between an electric vehicle and a charger: **ISO 15118-20** (DC and AC, Dynamic and Scheduled, with bidirectional power transfer), **ISO 15118-2** (DC and AC), **DIN SPEC 70121** and the **SLAC** bring-up. It has both ends of the link, the vehicle side (EVCC) and the charger side (EVSE/SECC), as portable cores you put your own platform under, EXI codecs generated from the standards' schemas, and simulators, scenarios and a web UI to test against.

The cores are `no_std`, allocation-free and **sans-IO**: it never touches a socket, a clock or a thread. You feed one events (bytes received, a timer fired, the vehicle did something) and carry out what it returns (bytes to send, timers to arm, things the host should do). Your platform decides how.

- Getting a checkout to build and test: [docs/getting-started.md](docs/getting-started.md)
- Design decisions and open work: [PLAN.md](PLAN.md)
- Where the standards contradict themselves or are silent, and what we decided: [docs/standards-notes.md](docs/standards-notes.md)

## Building your own EVCC

You write three things: the **vehicle** (`Ev`), the **loop** that connects the machine to your transport, and the **bring-up** around it. Start by reading the docs of the module and the trait named in each step (`cargo doc --open -p evcc-core`).

### 1. Describe the vehicle: `evcc_core::ev::Ev`

[`crates/evcc/core/src/ev.rs`](crates/evcc/core/src/ev.rs). The session asks the trait for facts; it never waits inside a call. Required: `evcc_id`, `now_micros`, `dc_connector`, `dc_limits`, `battery_voltage`, `inlet_voltage`, `schedule_request`. The rest has defaults (AC, discharge, contract, state of charge).

**Capabilities and present limits are separate calls.** They behave differently, so the trait keeps them apart:

| | Asked | Meaning | Methods |
|---|---|---|---|
| **Discovery** (capabilities) | once, in the charge parameter discovery | what the vehicle can *ever* take: its envelope | `dc_limits`, `dc_discharge_limits`, `ac20_limits`, `ac20_discharge_limits`, `ac_limits` (ISO 15118-2) |
| **Loop** (present limits) | before every charge loop request | what it can take *now*: temperature, state of charge, derating | `dc_loop_limits`, `dc_discharge_loop_limits`, `ac20_loop_limits`, `ac20_discharge_loop_limits`, `ac_loop_limits` |

The loop methods default to the discovery ones. With `Config::enforce_cpd_limits` (on by default) the session keeps what you return inside the announced envelope, as the standard requires (V2G20-2198, 2199): maximums no higher, minimums no lower. Turn it off to test a charger with values outside it.

Also in the trait: `schedule_request` (energy needs and departure time), `ac_present_power` (what the on-board charger draws now, reported in every AC loop request), `contract` (Plug & Charge: the certificate chain and the signing, which stays in your secure element or file; see `Contract`), `present_soc`.

Power on AC is per line (`Phases`: L1, optional L2 and L3). A single-phase car on a three-phase charger and a symmetric three-phase car are handled as V2G20-1815 to 1822 say.

### 2. Pick the protocol and its configuration

| Protocol | Machine | Configuration |
|---|---|---|
| ISO 15118-20 | `evcc_core::session::Session` | `Config::DC_20`, `DC_BPT_20`, `AC_20`, `AC_BPT_20`; `.scheduled()` for Scheduled control mode |
| ISO 15118-2 | `evcc_core::iso2::Session` | `iso2::Config::ISO2_DC`, `iso2::Config::ac(three_phase)` |
| DIN SPEC 70121 | `evcc_core::din::Session` | `din::Config::DIN` |

All three implement `evcc_core::machine::Machine`, so the loop below is the same for each. Create one with `Session::new(config, ev, log, Buffers { rx, tx, arena })`: the three buffers are yours (no allocation in the core). `rx` holds one V2GTP frame, `tx` the outgoing one, `arena` the decoded message (a few kB; 8 kB / 8 kB / 32 kB is plenty on a PC).

`log` is a `LogSink` (`evcc_core::log`): it receives a `Record` for every message, state change and rule the charger broke. `NoLog` drops them.

ISO 15118-20 requires TLS 1.3; the session refuses a plain connection unless `Config::allow_unsecured` is set (tests only).

### 3. Run the machine: the host contract

Documented at the top of [`session/mod.rs`](crates/evcc/core/src/session/mod.rs). In short:

1. Connect, then `machine.handle(Event::Start { secured })`.
2. Read bytes straight into `machine.rx_space()`, then `handle(Event::Received(n))`. One call handles one frame: while `machine.frame_pending()` (a second frame came in the same read; they do when a side stream runs beside the main stream), call `handle(Event::Received(0))` and carry out its step too, before you wait for the stream again.
3. After **every** call carry out the returned `Step`: write `machine.tx()[..send]` to the stream; apply `timers` (`TimerCmd::Arm { timer, ms }` replaces a pending expiry of that timer, `Cancel` removes it); handle `notice`; close the connection when `end` is set.
4. When a timer fires, `handle(Event::TimerExpired(timer))`.

`Notice`s are how the machine asks the vehicle to *do* something, and you answer with an `Event`:

| Notice | You | Then send |
|---|---|---|
| `CloseContactors` (DC, after pre-charge) | close the contactors | `Event::ContactorsClosed` |
| `StopEnergyTransfer` | ramp the current to zero | `Event::EnergyTransferStopped` |
| `OpenContactors` (DC) | open them | `Event::ContactorsOpened` |
| `ControlPilot(state)` (DIN, ISO 15118-2; -20 with `Config::control_pilot`) | change the control pilot | `Event::Pilot` with the state measured (the session waits for it; `Event::ControlPilotChanged` alone is not enough) |
| `ChargeLoop(status)` / `AcChargeLoop(status)` | apply the charger's current / target power | nothing |

Informational notices (`SessionSetUp`, `Authorized`, `ChargeParametersDiscovered`, `EnergyTransferStarted`, `StandbyEntered`, …) need no answer. You can also push events yourself while the charge loop runs: `StopRequested`, `PauseRequested`, `RenegotiateSchedule`, `RenegotiateServices`.

**On Linux you do not write this loop.** [`evcc_linux::run`](crates/evcc/linux/src/lib.rs) (and `run_gated`, which can hold the session at each state for a test platform) does steps 1 to 4 over any `Transport` (`TcpStream`, an OpenSSL `SslStream`) and calls your closure for each `Notice`:

```rust
let mut session = Session::new(Config::AC_20, my_ev, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
let outcome = evcc_linux::run(&mut stream, &mut session, true, &mut |notice| match notice {
    Notice::StopEnergyTransfer { .. } => { my_obc.ramp_down(); Some(Event::EnergyTransferStopped) }
    Notice::AcChargeLoop(s) => { my_obc.set_target_power(s.target_active_power); None }
    _ => None,
})?;
```

On an MCU, write the same loop around your own sockets and timers; `crates/evcc/linux/src/lib.rs` is about 200 lines and is the reference.

**The control pilot: you watch it, the machine tells you what to watch.** The CP can change at any moment and has to be acted on at once: the standards give the vehicle 50 ms to react to an emergency (IEC 61851-23 CC.4.3) and the charger 10 ms. That reaction is yours, in the fastest path you have (an interrupt, a comparator, a tight loop), without a trip through the session machine. What the machine adds is the *context*, which changes during the session: whether the pilot is *supposed* to leave C, or the oscillator to go off, depends on where the session is.

- **Arm and disarm.** Every `Step` can carry `pilot: Option<control_pilot::Guard>`, published when the session starts, whenever the expectation changes and, disarmed, when it ends (`Session::pilot_guard()` has the current one). A guard is a few plain conditions your monitor can implement itself or call as `guard.judge(reading)` (see [`control-pilot`](crates/pilot/src/lib.rs)): `hold_ready` (state C or D has to hold: leaving it is an emergency), `hold_five_percent` (DC: the oscillator has to be 5 %; another duty cycle or off is an emergency), `static_stops_supply` (AC: a constant duty cycle is the supply stopping, no emergency), `close_on_fault` (A, E, F end the session and close the connection). `Guard::DISARMED` judges nothing.
- **What `judge` says.** `Verdict::Emergency`: do your emergency shutdown (hardware), then give the machine the reading with `Event::Pilot` (the session ends at once, **nothing is sent**: `EndReason::Emergency`, `Ended::Emergency`), and drop the connection. `Fault`: end the session and close the connection (`EndReason::PilotFault`). `SupplyStopped`: forward the reading; the vehicle stops in order.
- **Report every change** as `Event::Pilot(control_pilot::Reading { state, pwm })` anyway, also before `Start`: the machine uses it for what it has to know (the charger answers the cable check `FAILED` unless it measured C or D, and the welding detection and the session stop after the DC stop unless it measured B: V2G20-917 to 922; the vehicle's DIN and ISO 15118-2 machines ask you to set C and B with `Notice::ControlPilot`, the -20 machine with `Config::control_pilot`). The machine judges the readings it is told by the same rule, so a host that only reports and does not arm a monitor still gets the stop, only slower. Without a single reading none of the checks applies. In an AC session the vehicle going from C to B is how it stops the charge (IEC 61851-1), so the charger takes it as no emergency (only A, E and F end the session); the ISO 15118-2 vehicle asks for C with its Start request and for B with its Stop request (V2G2-847, 848).
- **During the shutdown** the guard lets the pilot change and the oscillator go off, as the sequences say (CC.3.4.2).

On Linux `evcc_linux::run_with_pilot` takes a `PilotSource`: it polls it every 2 ms while it waits for the socket, passes the guards to `PilotSource::arm`, and looks at the pilot first when the connection drops (an emergency is not a hang-up). For the simulators `evcc_linux::pilot::Wire` is a cable made of two small files (the vehicle's state, the charger's oscillator), which the scenario runner gives to both.

### 4. Bring-up around the session

**SLAC starts on the pilot, never before.** Only a duty cycle of 5 % announces high level communication (ISO 15118-3, IEC 61851-1 Annex A; the vehicle measures 3 to 7 % as 5 %, because the line's capacitance and inductance change the measured value). A nominal duty cycle (10 to 96 %) means basic charging: the vehicle must not start SLAC. The SLAC machine enforces this itself, so every host gets it:

- Report the pilot with `slac::Event::Pilot(Reading { state, pwm })`: the first reading, every change, also before `Event::Start`. `Start` says "this vehicle wants SLAC"; the machine sends nothing until it has seen 5 % in state B, C or D, then begins (`Step::pilot` is then a guard, see below). Duty cycle values that are neither 5 % nor nominal (8 %, an oscillator that is off, 97 %) keep it waiting.
- A nominal duty cycle ends it with `Outcome::BasicCharging`: no matching, no high level communication. **What follows is yours**: charge by IEC 61851-1 alone (the vehicle's current limit is the duty cycle; `Pwm::Duty` is the reading), or do not charge if the vehicle cannot. The toolkit has no basic charging mode (the host closes the contactors and limits the current); `Pwm::offered_current_tenths()` gives the current a duty cycle offers (IEC 61851-1 Table A.7), and `evcc-sim` is an example: it asks for energy with state C, takes it for `basic_charge_ms` and stops with B.
- While it is matching, state A (unplugged), E, F (the charger signals an error or restarts the connection, V2G3-M06-05, 07) or an invalid voltage end it with `Outcome::Aborted(state)`; the guard it publishes is `close_on_fault`, so a monitor in an interrupt can end the matching without waiting for your loop. A change of the duty cycle does **not** interrupt a matching (V2G3-M06-15). Start over after `Aborted`: send `Event::Start` again; the machine waits for 5 % once more (the sim does exactly that).
- `Config::pilot_wait_ms` bounds the wait for 5 % (`Failure::NoFivePercent`); 0 waits as long as the car is plugged in. `Config::pilot_gate = false` is for a bench without a pilot (a veth pair): the machine begins at `Start`, and `evcc_linux::slac::run` does this for you.
- On Linux the pilot driver is the host's. Implement `evcc_linux::slac::ControlPilot` (which includes `evcc_linux::pilot::PilotSource`): `poll()` returns the latest reading when it changed (it is called every 2 ms while the loop waits for the socket, so an interrupt or a capture unit just keeps the last measurement there), `arm(guard)` receives the guard of the machine for a hardware monitor, `toggle(Toggle)` makes the B-C-B sequence of the validation (state B to C and back, `state_ms` each; blocking is fine). `evcc-sim` has `SimPilot`, which reads the simulated cable.
- After the match the session machines take over the pilot watching (the guards of section 3). They need the same readings: keep reporting them.

All of it sans-IO state machines too, driven the same way:

- **SLAC** (powerline matching): `evcc_core::slac::Machine`; on Linux `evcc_linux::slac::run_with_pilot` over a `PacketSocket`. After matching it hands the network key to your PLC modem through the `Modem` trait: `QcaModem` implements it for the QCA7000 family (key and version checked on a QCA7005; the link status and the vendor key message only against a fake modem), `NoModem` is for a modem that is already configured. **The control pilot gates it**, see below.
- **SDP** (find the charger): `evcc_core::sdp::Client`; on Linux `evcc_linux::sdp::{socket, discover, connect}`.
- **TLS 1.3** (required for -20): `evcc_linux::tls` (OpenSSL: `Credentials`, `client_context`, `connect`, with the certificate profile and OCSP checks of Annex B). On other platforms bring your own TLS and give the core a byte stream and the `secured` flag.
- **Plug & Charge**: implement `Contract` yourself (or use `evcc_linux::pnc::PemContract` from PEM files).

The order is SLAC, SDP, TCP, TLS, session. [`crates/sims/evcc-sim/src/main.rs`](crates/sims/evcc-sim/src/main.rs) does exactly that and is the shortest complete example; `SimulatedEv` in [`crates/sims/evcc-sim/src/lib.rs`](crates/sims/evcc-sim/src/lib.rs) is a worked `Ev`.

## Building your own EVSE

The charger side is the mirror image: [`crates/evse/core`](crates/evse/core) is the ISO 15118-20 session machine of the SECC (`evse_core::Session`, sans-IO and `no_std` like the vehicle's), and the charger is your implementation of [`evse_core::Secc`](crates/evse/core/src/secc.rs): what it offers (`energy_services`, `parameter_sets`, `authorization_services`), what it delivers (`dc_charge_parameters`, `dc_loop`, `ac_loop`, the contactors in `power_delivery`), what takes time (`authorize`, `cable_check`, `schedule_ready` answer with `Progress::Ongoing` and are asked again when the vehicle repeats its request), its meter (`meter_info`, `receipt`) and the verification of signatures (the machine hands the host the stream a signature covers; it contains no crypto). Every method has a default, so a start is a charger that accepts and does nothing. DIN SPEC 70121 and ISO 15118-2 have machines of their own in the same crate, `evse_core::din::Session` behind `DinSecc` and `evse_core::iso2::Session` behind `Iso2Secc`, built the same way (the host does the `SupportedAppProtocol` negotiation and starts the machine of the protocol it chose; `crates/evse/core/tests/din_iso2_against_evcc.rs` shows both against the vehicle machines). The host contract is in the crate docs; `crates/evse/core/tests/against_evcc.rs` is a complete example with a charger that delivers what the vehicle asks, and [`crates/sims/evse-sim`](crates/sims/evse-sim/src/secc.rs) is the full one (settings, faults, live measurements, a meter, Plug & Charge verification with OpenSSL).

**The charger's pilot and SLAC.** The charger makes the pilot signal; the cores tell it what to make and watch what the vehicle does. `evse_core::slac::Machine` is the charger's matching (ISO 15118-3 Annex A.9): the host feeds `Event::Frame` (the vehicle's messages and the modem's `CM_ATTEN_PROFILE.IND`), `Event::Pilot(Reading)` (the reading, also the first one: a change from A, E or F to B, C or D triggers the matching, V2G3-M06-11), timers, and what the modem and the higher layers report (`LinkUp`, `LinkDown`, `LinkError`, `LinkTerminate`, `LinkPause`, `Wake`). It answers with a `Step`: a frame, `set_key` for your modem, `toggles` (start counting BCB-Toggles on the pilot for `ms`, then read the count back with `Event::Toggles`), timers, a `Notice` (`D-LINK_READY.indication`) and `pilot: Option<PilotRequest>`: `ErrorCycle` (state E or F for `ms`, through X1 first if the oscillator is on) and `X1` (oscillator off, no more retries, V2G3-M06-07). Carrying out a `PilotRequest`, and the oscillator itself (5 % from the start of the data link setup until the end of the session, V2G2-915, V2G20-1408; the host sets it), are yours. A change from 5 % to a nominal duty cycle goes through E/F or X1 as the standard says; `evse-sim`'s `sequence` module is a worked host. The session machines watch the vehicle's states with `Event::Pilot` as described in section 3, and publish the `Guard` for your 10 ms emergency monitor.

## Where things are

| | |
|---|---|
| `crates/evcc/core` | the machines: sessions, SDP, SLAC, the `Ev` trait, logging. `no_std` |
| `crates/evse/core` | the charger's session machine (ISO 15118-20) behind the `Secc` trait. `no_std` |
| `crates/evcc/linux` | Linux adapter: sockets, OpenSSL, raw packet sockets, the QCA modem driver, the run loop |
| `crates/codec/*` | EXI codecs generated from the standards' schemas (`exi-gen`), V2GTP/SDP, HomePlug MMEs; checked byte for byte against EXIficient |
| `crates/sims/evcc-sim`, `evse-sim` | a simulated vehicle and charger (with fault injection) |
| `crates/sims/scenarios`, `web` | scenario runner in network namespaces, with interop runs against the Python `iso15118` library, and a web page to step through sessions |

How the codec compares with EXIficient, OpenV2G and libcbv2g (round trip, speed, memory, code size): [crates/codec/exi-gen/benchmarks](crates/codec/exi-gen/benchmarks/RESULTS.md).

Test it against something: `cargo xtask linux-test` (everything), `cargo xtask interop` (the Python library as charger), `cargo xtask web` (the web page).

## The standards are not included

The standards (ISO 15118-2, -3, -20, DIN SPEC 70121, and their XML schemas) are not part of this repository and are not covered by its license. Get them from ISO and DIN. `cargo xtask setup` downloads the schemas into `data/` and `standards/` (both gitignored) to regenerate the codecs; the generated codecs in `crates/codec/*/src/generated/` are committed so a fresh clone builds without them. The generator copies no text from the schemas. Requirement ids in comments and `docs/` only point into the standards.

## License

Licensed under either of [0BSD](LICENSE-0BSD) or [Apache License 2.0](LICENSE-APACHE), at your option. Unless you state otherwise, a contribution you submit is licensed under both, without further terms.
