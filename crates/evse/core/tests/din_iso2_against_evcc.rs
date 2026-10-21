//! The DIN SPEC 70121 and ISO 15118-2 charger machines against the vehicle machines of `evcc-core`, in memory.
//! The `SupportedAppProtocol` negotiation is the host's, so the harness answers it.

use std::collections::VecDeque;

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::NoLog;
use evcc_core::machine::Machine;
use evcc_core::session::{Buffers, EndReason, Event, Notice};
use evcc_core::{Timer, TimerCmd};
use evse_core::din::{self, DinSecc};
use evse_core::iso2::{self, Iso2Secc};
use iso15118_20::common_types::RationalNumber;

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

struct TestEv;

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn now_micros(&self) -> u64 {
        0
    }
    fn dc_connector(&self) -> DcConnector {
        DcConnector::Extended
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

/// Runs a vehicle machine against a charger: `feed` takes the bytes the vehicle sent and returns the bytes to give back and
/// whether the charger closes. Returns how the vehicle ended and how many charge loop answers it saw.
fn drive(vehicle: &mut dyn Machine, feed: impl FnMut(ToCharger<'_>) -> (Vec<u8>, bool), stop_after: u32) -> (Option<EndReason>, u32) {
    let (end, loops, _) = drive_host(vehicle, feed, stop_after, Host::default());
    (end, loops)
}

/// What reaches the charger.
enum ToCharger<'a> {
    Bytes(&'a [u8]),
    Pilot(control_pilot::Reading),
}

/// What the host tells the charger about the control pilot.
#[derive(Clone, Copy, Default)]
struct Host {
    /// Report the pilot the vehicle was told to set (C before the cable check, B after the contactors opened).
    report: bool,
    skip_c: bool,
    skip_b: bool,
    /// A reading of its own once the n-th charge loop answer is out.
    inject: Option<(u32, control_pilot::Reading)>,
}

/// As [`drive`], with a host that reports the pilot. Returns also whether the charger closed the connection.
fn drive_host(vehicle: &mut dyn Machine, mut feed: impl FnMut(ToCharger<'_>) -> (Vec<u8>, bool), stop_after: u32, host: Host) -> (Option<EndReason>, u32, bool) {
    if host.report {
        feed(ToCharger::Pilot(control_pilot::Reading::CONNECTED));
    }
    let mut closed = false;
    let sap = |vehicle: &mut dyn Machine, frames: Vec<u8>| -> Event {
        vehicle.rx_space()[..frames.len()].copy_from_slice(&frames);
        Event::Received(frames.len())
    };
    let mut queue: VecDeque<Event> = VecDeque::from([Event::Start { secured: false }]);
    let (mut loops, mut negotiated) = (0, false);
    for _ in 0..500 {
        let Some(event) = queue.pop_front() else { break };
        let step = vehicle.handle(event);
        if let Some(reason) = step.end {
            return (Some(reason), loops, closed);
        }
        if let Some(n) = step.send {
            let bytes = vehicle.tx()[..n].to_vec();
            let reply = if !negotiated {
                // The negotiation: the first protocol offered, schema id 1.
                negotiated = true;
                let res = app_protocol::SupportedAppProtocolRes { response_code: app_protocol::ResponseCode::OkSuccessfulNegotiation, schema_id: Some(1) };
                let mut buf = vec![0u8; 64];
                let len = res.encode(&mut buf[v2gtp::HEADER_LEN..]).unwrap();
                v2gtp::write_header(&mut buf, v2gtp::PayloadType::SAP, len).unwrap();
                buf.truncate(v2gtp::HEADER_LEN + len);
                (buf, false)
            } else {
                let r = feed(ToCharger::Bytes(&bytes));
                closed |= r.1;
                r
            };
            if !reply.0.is_empty() {
                let e = sap(vehicle, reply.0);
                queue.push_back(e);
            }
        }
        let reply = match step.notice {
            Some(Notice::CloseContactors) => Some(Event::ContactorsClosed),
            Some(Notice::StopEnergyTransfer { .. }) => Some(Event::EnergyTransferStopped),
            Some(Notice::OpenContactors) => Some(Event::ContactorsOpened),
            Some(Notice::ControlPilot(p)) => {
                let (state, skip) = match p {
                    evcc_core::session::ControlPilot::B => (control_pilot::State::B, host.skip_b),
                    evcc_core::session::ControlPilot::C => (control_pilot::State::C, host.skip_c),
                };
                let reading = control_pilot::Reading { state, pwm: control_pilot::Pwm::Duty(5) };
                if host.report && !skip
                    && feed(ToCharger::Pilot(reading)).1 {
                        return (None, loops, true);
                    }
                // The vehicle measures what it set; that is what it waits for.
                Some(Event::Pilot(reading))
            }
            Some(Notice::ChargeLoop(_) | Notice::AcChargeLoop(_)) => {
                loops += 1;
                if let Some((_, reading)) = host.inject.filter(|(n, _)| *n == loops)
                    && feed(ToCharger::Pilot(reading)).1 {
                        // The charger closed the connection: the vehicle gets nothing more.
                        return (None, loops, true);
                    }
                (loops == stop_after).then_some(Event::StopRequested)
            }
            _ => None,
        };
        if let Some(e) = reply {
            queue.push_front(e);
        }
        if step.timers.iter().any(|c| matches!(c, TimerCmd::Arm { timer: Timer::Poll, .. })) {
            queue.push_back(Event::TimerExpired(Timer::Poll));
        }
    }
    (None, loops, closed)
}

/// A DIN charger that accepts and delivers the limits it is given.
#[derive(Default)]
struct DinCharger {
    cable_checks: u32,
    loops: u32,
    ended: bool,
    how: Option<evse_core::secc::Ended>,
}

impl DinSecc for DinCharger {
    fn charge_parameters(&mut self, _ev: &din70121::types::DcEvChargeParameter) -> evse_core::Progress<(din::ResponseCode, din::Offer)> {
        use din70121::types::{DcEvseChargeParameter, DcEvseStatus, DcEvseStatusCode, EvseNotification, PhysicalValue, UnitSymbol};
        let pv = |x, u| PhysicalValue::from_f64(x, u);
        evse_core::Progress::Done((
            din::ResponseCode::Ok,
            din::Offer {
                parameter: DcEvseChargeParameter {
                    dc_evse_status: DcEvseStatus { evse_isolation_status: None, evse_status_code: DcEvseStatusCode::EvseReady, notification_max_delay: 0, evse_notification: EvseNotification::None },
                    evse_maximum_current_limit: pv(300.0, UnitSymbol::A),
                    evse_maximum_power_limit: Some(pv(150_000.0, UnitSymbol::W)),
                    evse_maximum_voltage_limit: pv(920.0, UnitSymbol::V),
                    evse_minimum_current_limit: pv(1.0, UnitSymbol::A),
                    evse_minimum_voltage_limit: pv(150.0, UnitSymbol::V),
                    evse_current_regulation_tolerance: None,
                    evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
                    evse_energy_to_be_delivered: None,
                },
                p_max_w: i16::MAX,
            },
        ))
    }
    fn cable_check(&mut self) -> evse_core::Progress<din::ResponseCode> {
        self.cable_checks += 1;
        if self.cable_checks < 2 { evse_core::Progress::Ongoing } else { evse_core::Progress::Done(din::ResponseCode::Ok) }
    }
    fn current_demand(&mut self, request: &din70121::body::CurrentDemandReq, _announced: &din70121::types::DcEvChargeParameter) -> din::LoopAnswer {
        self.loops += 1;
        din::LoopAnswer {
            response_code: din::ResponseCode::Ok,
            shutdown: false,
            present_voltage: request.ev_target_voltage,
            present_current: request.ev_target_current,
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            power_limit_achieved: false,
            maximum_voltage: None,
            maximum_current: None,
            maximum_power: None,
        }
    }
    fn session_ended(&mut self, how: evse_core::secc::Ended) {
        self.ended = true;
        self.how = Some(how);
    }
}

#[test]
fn a_din_session_runs_to_the_stop() {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut vehicle = evcc_core::din::Session::new(evcc_core::din::Config::DIN, TestEv, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut crx, mut ctx, mut carena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut charger = din::Session::new(DinCharger::default());
    let feed = |to: ToCharger<'_>| {
        let mut buffers = din::Buffers { rx: &mut crx, tx: &mut ctx, arena: &mut carena };
        let step = match to {
            ToCharger::Bytes(bytes) => {
                charger.rx_space(buffers.rx)[..bytes.len()].copy_from_slice(bytes);
                charger.handle(evse_core::Event::Received(bytes.len()), &mut buffers)
            }
            ToCharger::Pilot(r) => charger.handle(evse_core::Event::Pilot(r), &mut buffers),
        };
        (ctx[..step.send].to_vec(), step.close)
    };
    let (end, loops) = drive(&mut vehicle, feed, 3);
    assert_eq!(end, Some(EndReason::Completed));
    assert_eq!(loops, 3);
    assert_eq!(charger.state(), din::State::Stopped);
    assert_eq!(charger.secc().cable_checks, 2, "the cable check was polled once");
    assert_eq!(charger.secc().loops, 3);
    assert!(charger.secc().ended);
}

/// An ISO 15118-2 charger, DC or AC.
#[derive(Default)]
struct Iso2Charger {
    loops: u32,
    cable_checks: u32,
    how: Option<evse_core::secc::Ended>,
}

impl Iso2Secc for Iso2Charger {
    fn cable_check(&mut self) -> evse_core::Progress<iso2::ResponseCode> {
        self.cable_checks += 1;
        evse_core::Progress::Done(iso2::ResponseCode::Ok)
    }
    fn session_ended(&mut self, how: evse_core::secc::Ended) {
        self.how = Some(how);
    }
    fn energy_modes(&self) -> &[iso15118_2::types::EnergyTransferMode] {
        &[iso15118_2::types::EnergyTransferMode::DcExtended, iso15118_2::types::EnergyTransferMode::AcThreePhaseCore]
    }
    fn charge_parameters(&mut self, request: &iso15118_2::body::ChargeParameterDiscoveryReq, _renegotiating: bool) -> evse_core::Progress<(iso2::ResponseCode, iso2::Offer)> {
        use iso15118_2::types::{AcEvseChargeParameter, AcEvseStatus, DcEvseChargeParameter, DcEvseStatus, DcEvseStatusCode, EnergyTransferMode, EvseNotification, PhysicalValue, UnitSymbol};
        let pv = |x, u| PhysicalValue::from_f64(x, u);
        let ac = request.requested_energy_transfer_mode == EnergyTransferMode::AcThreePhaseCore;
        let parameter = if ac {
            iso15118_2::body::EvseChargeParameterChoice::Ac(AcEvseChargeParameter {
                ac_evse_status: AcEvseStatus { notification_max_delay: 0, evse_notification: EvseNotification::None, rcd: false },
                evse_nominal_voltage: pv(230.0, UnitSymbol::V),
                evse_max_current: pv(32.0, UnitSymbol::A),
            })
        } else {
            iso15118_2::body::EvseChargeParameterChoice::Dc(DcEvseChargeParameter {
                dc_evse_status: DcEvseStatus { notification_max_delay: 0, evse_notification: EvseNotification::None, evse_isolation_status: None, evse_status_code: DcEvseStatusCode::EvseReady },
                evse_maximum_current_limit: pv(300.0, UnitSymbol::A),
                evse_maximum_power_limit: pv(150_000.0, UnitSymbol::W),
                evse_maximum_voltage_limit: pv(920.0, UnitSymbol::V),
                evse_minimum_current_limit: pv(1.0, UnitSymbol::A),
                evse_minimum_voltage_limit: pv(150.0, UnitSymbol::V),
                evse_current_regulation_tolerance: None,
                evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
                evse_energy_to_be_delivered: None,
            })
        };
        evse_core::Progress::Done((iso2::ResponseCode::Ok, iso2::Offer { parameter, p_max: pv(if ac { 22_080.0 } else { 150_000.0 }, UnitSymbol::W) }))
    }
    fn dc_loop(&mut self, request: &iso15118_2::body::CurrentDemandReq, _announced: Option<iso15118_2::types::PhysicalValue>) -> iso2::DcLoopAnswer {
        self.loops += 1;
        iso2::DcLoopAnswer {
            response_code: iso2::ResponseCode::Ok,
            shutdown: false,
            notification: iso15118_2::types::EvseNotification::None,
            present_voltage: request.ev_target_voltage,
            present_current: request.ev_target_current,
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            power_limit_achieved: false,
            maximum_voltage: None,
            maximum_current: None,
            maximum_power: None,
            meter_info: None,
            receipt_required: None,
        }
    }
    fn ac_loop(&mut self, _request: &iso15118_2::body::ChargingStatusReq) -> iso2::AcLoopAnswer {
        self.loops += 1;
        iso2::AcLoopAnswer { response_code: iso2::ResponseCode::Ok, notification: iso15118_2::types::EvseNotification::None, max_current: None, meter_info: None, receipt_required: None }
    }
}

fn iso2_session(config: evcc_core::iso2::Config) -> (u32, iso2::State, Option<EndReason>) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut vehicle = evcc_core::iso2::Session::new(config, TestEv, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut crx, mut ctx, mut carena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 65536]);
    let mut charger = iso2::Session::new(Iso2Charger::default());
    let feed = |to: ToCharger<'_>| {
        let mut buffers = iso2::Buffers { rx: &mut crx, tx: &mut ctx, arena: &mut carena };
        let step = match to {
            ToCharger::Bytes(bytes) => {
                charger.rx_space(buffers.rx)[..bytes.len()].copy_from_slice(bytes);
                charger.handle(evse_core::Event::Received(bytes.len()), &mut buffers)
            }
            ToCharger::Pilot(r) => charger.handle(evse_core::Event::Pilot(r), &mut buffers),
        };
        (ctx[..step.send].to_vec(), step.close)
    };
    let (end, _) = drive(&mut vehicle, feed, 2);
    (charger.secc().loops, charger.state(), end)
}

#[test]
fn an_iso2_dc_session_runs_to_the_stop() {
    let (loops, state, end) = iso2_session(evcc_core::iso2::Config::ISO2_DC);
    assert_eq!((end, state), (Some(EndReason::Completed), iso2::State::Stopped));
    assert_eq!(loops, 2);
}

#[test]
fn an_iso2_ac_session_runs_to_the_stop() {
    let (loops, state, end) = iso2_session(evcc_core::iso2::Config::ac(true));
    assert_eq!((end, state), (Some(EndReason::Completed), iso2::State::Stopped));
    assert_eq!(loops, 2);
}

// ---- the control pilot

/// DIN: the vehicle against the charger, with a host that reports the pilot. Returns how the vehicle ended, whether the
/// charger closed, its state and its record.
fn din_pilot(host: Host, stop_after: u32) -> (Option<EndReason>, bool, din::State, DinCharger) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut vehicle = evcc_core::din::Session::new(evcc_core::din::Config::DIN, TestEv, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut crx, mut ctx, mut carena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut charger = din::Session::new(DinCharger::default());
    let feed = |to: ToCharger<'_>| {
        let mut buffers = din::Buffers { rx: &mut crx, tx: &mut ctx, arena: &mut carena };
        let step = match to {
            ToCharger::Bytes(bytes) => {
                charger.rx_space(buffers.rx)[..bytes.len()].copy_from_slice(bytes);
                charger.handle(evse_core::Event::Received(bytes.len()), &mut buffers)
            }
            ToCharger::Pilot(r) => charger.handle(evse_core::Event::Pilot(r), &mut buffers),
        };
        (ctx[..step.send].to_vec(), step.close)
    };
    let (end, _, closed) = drive_host(&mut vehicle, feed, stop_after, host);
    let state = charger.state();
    (end, closed, state, std::mem::take(charger.secc_mut()))
}

fn iso2_pilot(host: Host, stop_after: u32) -> (Option<EndReason>, bool, iso2::State, Iso2Charger) {
    iso2_pilot_with(evcc_core::iso2::Config::ISO2_DC, host, stop_after)
}

fn iso2_pilot_with(config: evcc_core::iso2::Config, host: Host, stop_after: u32) -> (Option<EndReason>, bool, iso2::State, Iso2Charger) {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut vehicle = evcc_core::iso2::Session::new(config, TestEv, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let (mut crx, mut ctx, mut carena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 65536]);
    let mut charger = iso2::Session::new(Iso2Charger::default());
    let feed = |to: ToCharger<'_>| {
        let mut buffers = iso2::Buffers { rx: &mut crx, tx: &mut ctx, arena: &mut carena };
        let step = match to {
            ToCharger::Bytes(bytes) => {
                charger.rx_space(buffers.rx)[..bytes.len()].copy_from_slice(bytes);
                charger.handle(evse_core::Event::Received(bytes.len()), &mut buffers)
            }
            ToCharger::Pilot(r) => charger.handle(evse_core::Event::Pilot(r), &mut buffers),
        };
        (ctx[..step.send].to_vec(), step.close)
    };
    let (end, _, closed) = drive_host(&mut vehicle, feed, stop_after, host);
    let state = charger.state();
    (end, closed, state, std::mem::take(charger.secc_mut()))
}

#[test]
fn din_and_iso2_chargers_follow_the_pilot_of_a_normal_session() {
    let (end, _, state, charger) = din_pilot(Host { report: true, ..Default::default() }, 3);
    assert_eq!((end, state), (Some(EndReason::Completed), din::State::Stopped));
    assert!(matches!(charger.how, Some(evse_core::secc::Ended::Stopped(_))), "{:?}", charger.how);
    let (end, _, state, charger) = iso2_pilot(Host { report: true, ..Default::default() }, 3);
    assert_eq!((end, state), (Some(EndReason::Completed), iso2::State::Stopped));
    assert!(matches!(charger.how, Some(evse_core::secc::Ended::Stopped(_))), "{:?}", charger.how);
}

#[test]
fn din_and_iso2_chargers_fail_the_cable_check_without_c_or_d() {
    let host = Host { report: true, skip_c: true, ..Default::default() };
    let (end, closed, _, charger) = din_pilot(host, 3);
    assert!(matches!(end, Some(EndReason::RejectedDin(din70121::types::ResponseCode::Failed))), "{end:?}");
    assert!(closed);
    assert_eq!(charger.cable_checks, 0);
    let (end, closed, _, charger) = iso2_pilot(host, 3);
    assert!(matches!(end, Some(EndReason::RejectedIso2(iso15118_2::types::ResponseCode::Failed))), "{end:?}");
    assert!(closed);
    assert_eq!(charger.cable_checks, 0);
}

#[test]
fn din_and_iso2_chargers_fail_the_welding_detection_without_b() {
    let host = Host { report: true, skip_b: true, ..Default::default() };
    let (end, _, _, _) = din_pilot(host, 2);
    assert!(matches!(end, Some(EndReason::RejectedDin(din70121::types::ResponseCode::Failed))), "{end:?}");
    let (end, _, _, _) = iso2_pilot(host, 2);
    assert!(matches!(end, Some(EndReason::RejectedIso2(iso15118_2::types::ResponseCode::Failed))), "{end:?}");
}

#[test]
fn din_and_iso2_chargers_end_the_session_in_an_emergency_without_a_word() {
    use control_pilot::{Pwm, Reading, State as Cp};
    for reading in [Reading::CONNECTED, Reading { state: Cp::B, pwm: Pwm::Off }, Reading { state: Cp::E, pwm: Pwm::Off }] {
        let host = Host { report: true, inject: Some((2, reading)), ..Default::default() };
        let (end, closed, state, charger) = din_pilot(host, 100);
        assert_eq!((end, closed, state), (None, true, din::State::Closed));
        assert_eq!(charger.how, Some(evse_core::secc::Ended::Emergency(reading)));
        assert_eq!(charger.loops, 2, "no answer after the emergency");
        let (end, closed, state, charger) = iso2_pilot(host, 100);
        assert_eq!((end, closed, state), (None, true, iso2::State::Closed));
        assert_eq!(charger.how, Some(evse_core::secc::Ended::Emergency(reading)));
        assert_eq!(charger.loops, 2);
    }
}

#[test]
fn an_ac_session_with_the_pilot_reported_runs_to_the_end_and_the_charger_takes_c_and_b_as_the_vehicle_stopping() {
    // The vehicle sets C with its Start request and B with its Stop request (V2G2-847, 848); in AC that is no emergency (IEC 61851-1).
    let (end, closed, state, charger) = iso2_pilot_with(evcc_core::iso2::Config::ac(true), Host { report: true, ..Default::default() }, 2);
    assert_eq!(end, Some(EndReason::Completed));
    assert!(closed);
    assert_eq!(state, iso2::State::Stopped);
    assert!(matches!(charger.how, Some(evse_core::secc::Ended::Stopped(_))), "{:?}", charger.how);
}
