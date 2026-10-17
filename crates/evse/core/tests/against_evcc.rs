//! The charger machine against the vehicle machine of `evcc-core`, in memory: two implementations of
//! the same standard written separately, meeting over byte buffers (no sockets, no timers).

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::NoLog;
use evcc_core::session::{Buffers as EvBuffers, Config, EndReason, Event as EvEvent, Notice, Session as EvSession};
use evse_core::secc::{AcLoopAnswer, DcLoopAnswer, Progress, Secc};
use evse_core::{Buffers, Event, Session, State};
use iso15118_20::ac;
use iso15118_20::common::{Authorization, Parameter, ParameterChoice, ParameterSet, PowerSchedule, PowerScheduleEntry, PowerScheduleEntryList, ScheduleTuple, ChargingSchedule, Service};
use iso15118_20::common_types::{EvseNotification, EvseStatus, MeterInfo, RationalNumber, ResponseCode};
use iso15118_20::dc;

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

struct TestEv {
    clock: std::cell::Cell<u64>,
}

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn now_micros(&self) -> u64 {
        self.clock.set(self.clock.get() + 1);
        self.clock.get()
    }
    fn dc_connector(&self) -> DcConnector {
        DcConnector::Core
    }
    fn dc_limits(&self) -> DcLimits {
        DcLimits {
            maximum_charge_power: rn(3, 100),
            minimum_charge_power: rn(0, 100),
            maximum_charge_current: rn(0, 200),
            minimum_charge_current: rn(0, 1),
            maximum_voltage: rn(0, 450),
            minimum_voltage: rn(0, 200),
            target_soc: Some(80),
        }
    }
    fn battery_voltage(&self) -> RationalNumber {
        rn(0, 400)
    }
    fn inlet_voltage(&self) -> RationalNumber {
        rn(0, 400)
    }
    fn schedule_request(&self) -> ScheduleRequest {
        ScheduleRequest { departure_time: 3600, minimum_soc: None, target_soc: Some(80), target_energy: rn(3, 40), maximum_energy: rn(3, 60), minimum_energy: rn(0, 0) }
    }
}

static DC_PARAMS: [Parameter<'static>; 4] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static AC_PARAMS: [Parameter<'static>; 5] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "EVSENominalVoltage", choice: ParameterChoice::IntValue(400) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static AC_SCHEDULED_PARAMS: [Parameter<'static>; 5] = [
    Parameter { name: "Connector", choice: ParameterChoice::IntValue(2) },
    Parameter { name: "EVSENominalVoltage", choice: ParameterChoice::IntValue(400) },
    Parameter { name: "ControlMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "MobilityNeedsMode", choice: ParameterChoice::IntValue(1) },
    Parameter { name: "Pricing", choice: ParameterChoice::IntValue(0) },
];
static DC_SETS: [ParameterSet<'static>; 1] = [ParameterSet { parameter_set_id: 1, parameter: &DC_PARAMS }];
static AC_SETS: [ParameterSet<'static>; 2] = [ParameterSet { parameter_set_id: 1, parameter: &AC_PARAMS }, ParameterSet { parameter_set_id: 2, parameter: &AC_SCHEDULED_PARAMS }];
static SERVICES: [Service; 4] = [
    Service { service_id: 1, free_service: false },
    Service { service_id: 2, free_service: false },
    Service { service_id: 5, free_service: false },
    Service { service_id: 6, free_service: false },
];
static SCHEDULE_ENTRIES: [PowerScheduleEntry; 1] = [PowerScheduleEntry { duration: 3600, power: RationalNumber { exponent: 3, value: 11 }, power_l2: None, power_l3: None }];
static TUPLES: [ScheduleTuple<'static>; 1] = [ScheduleTuple {
    schedule_tuple_id: 1,
    charging_schedule: ChargingSchedule {
        power_schedule: PowerSchedule { time_anchor: 0, available_energy: None, power_tolerance: None, power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: &SCHEDULE_ENTRIES } },
        schedule: None,
    },
    discharging_schedule: None,
}];

/// A charger with fixed limits that delivers what the vehicle asks for.
#[derive(Default)]
struct TestCharger {
    loops: u32,
    cable_checks: u32,
    delivering: bool,
    confirmations: u32,
    ended: Option<evse_core::secc::Ended>,
    meter_id: String,
}

impl Secc for TestCharger {
    fn evse_id(&self) -> &str {
        "DE*TST*E1"
    }
    fn now_micros(&self) -> u64 {
        1_700_000_000_000_000
    }
    fn new_session_id(&mut self) -> [u8; 8] {
        [1, 2, 3, 4, 5, 6, 7, 8]
    }
    fn gen_challenge(&mut self) -> [u8; 16] {
        [9; 16]
    }
    fn authorization_services(&self) -> &[Authorization] {
        &[Authorization::Eim, Authorization::PnC]
    }
    fn energy_services(&self) -> &[Service] {
        &SERVICES
    }
    fn parameter_sets(&self, service_id: u16) -> &[ParameterSet<'_>] {
        match service_id {
            1 | 5 => &AC_SETS,
            2 | 6 => &DC_SETS,
            _ => &[],
        }
    }
    fn authorize(&mut self, request: evse_core::secc::Authorize<'_, '_>) -> Progress<ResponseCode> {
        // A real charger verifies the chain and the signature here (see `evse-sim`'s `pnc`).
        if let evse_core::secc::Authorize::PnC { header, fragment, challenge, mode } = request {
            assert!(header.signature.is_some() && !fragment.is_empty());
            assert_eq!(mode.gen_challenge, challenge);
        }
        Progress::Done(ResponseCode::Ok)
    }
    fn dc_charge_parameters(&mut self, bpt: bool, _request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        let base = dc::DcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 150),
            evse_minimum_charge_power: rn(0, 1000),
            evse_maximum_charge_current: rn(0, 300),
            evse_minimum_charge_current: rn(0, 2),
            evse_maximum_voltage: rn(0, 920),
            evse_minimum_voltage: rn(0, 150),
            evse_power_ramp_limitation: None,
        };
        Ok(if bpt {
            dc::DcCpdResEnergyTransferModeChoice::Bpt(dc::BptDcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_maximum_charge_current: base.evse_maximum_charge_current,
                evse_minimum_charge_current: base.evse_minimum_charge_current,
                evse_maximum_voltage: base.evse_maximum_voltage,
                evse_minimum_voltage: base.evse_minimum_voltage,
                evse_power_ramp_limitation: None,
                evse_maximum_discharge_power: rn(3, 11),
                evse_minimum_discharge_power: rn(0, 100),
                evse_maximum_discharge_current: rn(0, 30),
                evse_minimum_discharge_current: rn(0, 1),
            })
        } else {
            dc::DcCpdResEnergyTransferModeChoice::Base(base)
        })
    }
    fn ac_charge_parameters(&mut self, bpt: bool, _request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        let base = ac::AcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 4),
            evse_maximum_charge_power_l2: Some(rn(3, 4)),
            evse_maximum_charge_power_l3: Some(rn(3, 4)),
            evse_minimum_charge_power: rn(0, 1380),
            evse_minimum_charge_power_l2: Some(rn(0, 1380)),
            evse_minimum_charge_power_l3: Some(rn(0, 1380)),
            evse_nominal_frequency: rn(0, 50),
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: None,
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        };
        Ok(if bpt {
            ac::AcCpdResEnergyTransferModeChoice::Bpt(ac::BptAcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_maximum_charge_power_l2: base.evse_maximum_charge_power_l2,
                evse_maximum_charge_power_l3: base.evse_maximum_charge_power_l3,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_minimum_charge_power_l2: base.evse_minimum_charge_power_l2,
                evse_minimum_charge_power_l3: base.evse_minimum_charge_power_l3,
                evse_nominal_frequency: base.evse_nominal_frequency,
                maximum_power_asymmetry: None,
                evse_power_ramp_limitation: None,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
                evse_maximum_discharge_power: rn(3, 4),
                evse_maximum_discharge_power_l2: Some(rn(3, 4)),
                evse_maximum_discharge_power_l3: Some(rn(3, 4)),
                evse_minimum_discharge_power: rn(0, 500),
                evse_minimum_discharge_power_l2: Some(rn(0, 500)),
                evse_minimum_discharge_power_l3: Some(rn(0, 500)),
            })
        } else {
            ac::AcCpdResEnergyTransferModeChoice::Base(base)
        })
    }
    fn schedule_tuples(&self) -> &[ScheduleTuple<'_>] {
        &TUPLES
    }
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.cable_checks += 1;
        // One round of Ongoing, to see the polling work.
        if self.cable_checks < 2 { Progress::Ongoing } else { Progress::Done(ResponseCode::Ok) }
    }
    fn power_delivery(&mut self, request: &iso15118_20::common::PowerDeliveryReq<'_>) -> ResponseCode {
        self.delivering = request.charge_progress == iso15118_20::common::ChargeProgress::Start;
        ResponseCode::Ok
    }
    fn dc_loop(&mut self, request: &dc::DcChargeLoopReq<'_>, bpt: bool) -> Option<DcLoopAnswer> {
        self.loops += 1;
        let _ = bpt;
        let (maximum_current, mode) = match &request.cl_req_control_mode {
            dc::ClReqControlModeChoice::DynamicDc(m) => (m.ev_maximum_charge_current, dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
            })),
            dc::ClReqControlModeChoice::BptDynamicDc(m) => (m.ev_maximum_charge_current, dc::ClResControlModeChoice::BptDynamicDc(dc::BptDynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
                evse_maximum_discharge_power: rn(3, 11),
                evse_minimum_discharge_power: rn(0, 100),
                evse_maximum_discharge_current: rn(0, 30),
                evse_minimum_voltage: rn(0, 150),
            })),
            _ => return None,
        };
        Some(DcLoopAnswer {
            response_code: ResponseCode::Ok,
            present_current: maximum_current,
            present_voltage: request.ev_present_voltage,
            power_limit_achieved: false,
            current_limit_achieved: true,
            voltage_limit_achieved: false,
            control_mode: mode,
            status: (self.loops == 2).then_some(EvseStatus { notification_max_delay: 10, evse_notification: EvseNotification::MeteringConfirmation }),
        })
    }
    fn ac_loop(&mut self, request: &ac::AcChargeLoopReq<'_>, _bpt: bool) -> Option<AcLoopAnswer> {
        self.loops += 1;
        let power = rn(3, 4);
        let mode = match &request.cl_req_control_mode {
            ac::ClReqControlModeChoice::DynamicAc(_) => ac::ClResControlModeChoice::DynamicAc(ac::DynamicAcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_target_active_power: power,
                evse_target_active_power_l2: Some(power),
                evse_target_active_power_l3: Some(power),
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
            }),
            ac::ClReqControlModeChoice::ScheduledAc(_) => ac::ClResControlModeChoice::ScheduledAc(ac::ScheduledAcClResControlMode {
                evse_target_active_power: None,
                evse_target_active_power_l2: None,
                evse_target_active_power_l3: None,
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: Some(power),
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
            }),
            _ => return None,
        };
        Some(AcLoopAnswer { response_code: ResponseCode::Ok, target_frequency: None, control_mode: mode, status: None })
    }
    fn meter_info(&self, wanted: bool) -> Option<MeterInfo<'_>> {
        wanted.then(|| MeterInfo {
            meter_id: &self.meter_id,
            charged_energy_reading_wh: 1234 + u64::from(self.loops),
            bpt_discharged_energy_reading_wh: None,
            capacitive_energy_reading_va_rh: None,
            bpt_inductive_energy_reading_va_rh: None,
            meter_signature: None,
            meter_status: None,
            meter_timestamp: None,
        })
    }
    fn confirm_metering(&mut self, _request: &iso15118_20::common::MeteringConfirmationReq<'_>, signed: evse_core::secc::Signed<'_, '_>) -> ResponseCode {
        // The signature is there and covers a stream.
        assert!(signed.header.signature.is_some() && !signed.fragment.is_empty());
        self.confirmations += 1;
        ResponseCode::Ok
    }
    fn note(&mut self, text: core::fmt::Arguments<'_>) {
        if std::env::var_os("EVSE_TRACE").is_some() {
            eprintln!("[evse] {text}");
        }
    }
    fn session_ended(&mut self, how: evse_core::secc::Ended) {
        self.ended = Some(how);
    }
}

/// A vehicle's contract that signs with nothing: the charger under test accepts anything.
struct FakeContract;

impl evcc_core::ev::Contract for FakeContract {
    fn certificate(&self) -> &[u8] {
        &[1, 2, 3]
    }
    fn sub_certificate(&self, index: usize) -> Option<&[u8]> {
        // The chain has at least one sub-CA certificate (the list has a minimum of one).
        (index == 0).then_some(&[4, 5, 6][..])
    }
    fn signature_algorithm(&self) -> &str {
        iso15118_20::signature::ECDSA_SHA512
    }
    fn digest_algorithm(&self) -> &str {
        iso15118_20::signature::DIGEST_SHA512
    }
    fn digest(&self, _data: &[u8], out: &mut [u8]) -> Option<usize> {
        out[..64].fill(7);
        Some(64)
    }
    fn sign(&self, _data: &[u8], out: &mut [u8]) -> Option<usize> {
        out[..132].fill(5);
        Some(132)
    }
}

struct PnCEv(TestEv, FakeContract);

impl Ev for PnCEv {
    fn evcc_id(&self) -> &str {
        self.0.evcc_id()
    }
    fn now_micros(&self) -> u64 {
        self.0.now_micros()
    }
    fn dc_connector(&self) -> DcConnector {
        self.0.dc_connector()
    }
    fn dc_limits(&self) -> DcLimits {
        self.0.dc_limits()
    }
    fn battery_voltage(&self) -> RationalNumber {
        self.0.battery_voltage()
    }
    fn inlet_voltage(&self) -> RationalNumber {
        self.0.inlet_voltage()
    }
    fn schedule_request(&self) -> ScheduleRequest {
        self.0.schedule_request()
    }
    fn contract(&self) -> Option<&dyn evcc_core::ev::Contract> {
        Some(&self.1)
    }
}

/// The two machines and what passes between them.
struct Rig<'a, E: Ev> {
    vehicle: EvSession<'a, E, NoLog>,
    charger: Session<TestCharger>,
    charger_buffers: Buffers<'a>,
    ended: Option<EndReason>,
    loops: u32,
    stop_after: u32,
    closed: bool,
}

impl<E: Ev> Rig<'_, E> {
    /// Carries out what a vehicle step asks for: frames to the charger, its answers back, the
    /// notices the host answers.
    fn process(&mut self, step: evcc_core::session::Step) {
        if let Some(end) = step.end {
            self.ended = Some(end);
        }
        if let Some(n) = step.notice {
            let event = match n {
                Notice::CloseContactors => Some(EvEvent::ContactorsClosed),
                Notice::StopEnergyTransfer { .. } => Some(EvEvent::EnergyTransferStopped),
                Notice::OpenContactors => Some(EvEvent::ContactorsOpened),
                Notice::ChargeLoop(_) | Notice::AcChargeLoop(_) => {
                    self.loops += 1;
                    (self.loops == self.stop_after).then_some(EvEvent::StopRequested)
                }
                _ => None,
            };
            if let Some(e) = event {
                let s = self.vehicle.handle(e);
                self.process(s);
            }
        }
        if let Some(n) = step.send {
            let frames = self.vehicle.tx()[..n].to_vec();
            let space = self.charger.rx_space(self.charger_buffers.rx);
            space[..frames.len()].copy_from_slice(&frames);
            let reply = self.charger.handle(Event::Received(frames.len()), &mut self.charger_buffers);
            let bytes = self.charger_buffers.tx[..reply.send].to_vec();
            // One frame at a time, as a stream would deliver them.
            let mut at = 0;
            while at < bytes.len() {
                let len = 8 + u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
                self.vehicle.rx_space()[..len].copy_from_slice(&bytes[at..at + len]);
                let s = self.vehicle.handle(EvEvent::Received(len));
                self.process(s);
                at += len;
            }
            self.closed |= reply.close;
        }
    }
}

/// Runs the vehicle against the charger until one of them is done; `stop_after` charge loop answers.
fn run<E: Ev>(config: Config, ev: E, stop_after: u32) -> (EndReason, TestCharger, State) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let vehicle = EvSession::new(config, ev, NoLog, EvBuffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut srx, mut stx, mut sarena, mut sscratch) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768], vec![0u8; 8192]);
    let mut charger_buffers = Buffers { rx: &mut srx, tx: &mut stx, arena: &mut sarena, scratch: &mut sscratch };
    let mut charger = Session::new(TestCharger::default());
    charger.handle(Event::Connected { secured: true }, &mut charger_buffers);
    let mut rig = Rig { vehicle, charger, charger_buffers, ended: None, loops: 0, stop_after, closed: false };
    let start = rig.vehicle.handle(EvEvent::Start { secured: true });
    rig.process(start);
    for _ in 0..200 {
        if rig.ended.is_some() || rig.closed {
            break;
        }
        // Nothing in flight: the vehicle waits for its poll pause or loop interval.
        let s = rig.vehicle.handle(EvEvent::TimerExpired(evcc_core::Timer::Poll));
        rig.process(s);
    }
    let state = rig.charger.state();
    let ended = rig.ended.expect("the vehicle ended its session");
    (ended, std::mem::take(rig.charger.secc_mut()), state)
}

fn config(base: Config) -> Config {
    Config { poll_interval_ms: 0, charge_loop_interval_ms: 1, ..base }
}

#[test]
fn a_dc_session_runs_to_the_stop() {
    let (end, charger, state) = run(config(Config::DC_20), TestEv { clock: 1.into() }, 3);
    assert_eq!(end, EndReason::Completed);
    assert_eq!(state, State::Stopped);
    assert_eq!(charger.loops, 3);
    assert_eq!(charger.cable_checks, 2, "the cable check was polled once");
    assert!(!charger.delivering);
}

#[test]
fn a_bidirectional_dc_session() {
    let (end, charger, state) = run(config(Config::DC_BPT_20), TestEv { clock: 1.into() }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!(charger.loops, 2);
}

#[test]
fn an_ac_session_skips_the_dc_steps() {
    let (end, charger, state) = run(config(Config::AC_20), TestEv { clock: 1.into() }, 3);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!((charger.loops, charger.cable_checks), (3, 0));
}

#[test]
fn a_scheduled_ac_session_selects_the_offered_tuple() {
    let (end, charger, state) = run(config(Config::AC_20.scheduled()), TestEv { clock: 1.into() }, 2);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    assert_eq!(charger.loops, 2);
}

#[test]
fn a_plug_and_charge_session_confirms_the_metering() {
    let cfg = Config { prefer_pnc: true, request_meter_info: true, ..config(Config::DC_20) };
    let (end, charger, state) = run(cfg, PnCEv(TestEv { clock: 1.into() }, FakeContract), 4);
    assert_eq!((end, state), (EndReason::Completed, State::Stopped));
    // The notification came with the second answer; the vehicle signed and the charger took it.
    assert_eq!(charger.confirmations, 1);
}
