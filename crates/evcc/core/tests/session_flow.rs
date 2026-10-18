//! Drives the session machine against a scripted EVSE built from the same codecs (which are
//! themselves checked against EXIficient vectors).

use std::cell::Cell;
use std::rc::Rc;

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::{Level, LogSink, MsgKind, Record};
use evcc_core::session::*;
use evcc_core::{Timer, TimerCmd, Timers};
use app_protocol::{self, ResponseCode as SapCode, SupportedAppProtocolRes};
use exi::{Arena, DecodeError};
use iso15118_20::common::{
    self, AsResAuthorizationModeChoice, Authorization, AuthorizationRes, AuthorizationSetupRes, EimAsResAuthorizationMode,
    Parameter, ParameterChoice, ParameterSet, SelectedService, Service, ServiceDetailRes, ServiceDiscoveryRes,
    ServiceList, ServiceParameterList, ServiceSelectionRes, SessionSetupRes,
};
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, PayloadType};

thread_local! {
    /// What the vehicle measures at its inlet (volts); tests set it to "settle" pre-charge.
    static INLET: Cell<i16> = const { Cell::new(0) };
}

const BATTERY_V: i16 = 400;

thread_local! {
    /// The vehicle asks the host for the control pilot (`Config::control_pilot`).
    static PILOT: Cell<bool> = const { Cell::new(false) };
}

thread_local! {
    /// The maximum charge current the vehicle can take right now (amperes); 0: as announced.
    static LOOP_CURRENT: Cell<i16> = const { Cell::new(0) };
}

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

/// The vehicle. Every call to `now_micros` returns a larger value so tests can tell
/// messages apart.
#[derive(Clone)]
struct TestEv(Rc<Cell<u64>>);

impl TestEv {
    fn new() -> Self {
        Self(Rc::new(Cell::new(1_700_000_000)))
    }
}

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
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
    fn dc_loop_limits(&self) -> DcLimits {
        let now = LOOP_CURRENT.with(Cell::get);
        DcLimits { maximum_charge_current: if now > 0 { rn(0, now) } else { self.dc_limits().maximum_charge_current }, ..self.dc_limits() }
    }
    fn battery_voltage(&self) -> RationalNumber {
        rn(0, BATTERY_V)
    }
    fn inlet_voltage(&self) -> RationalNumber {
        rn(0, INLET.with(Cell::get))
    }
    fn schedule_request(&self) -> ScheduleRequest {
        ScheduleRequest {
            departure_time: 3600,
            minimum_soc: None,
            target_soc: Some(80),
            target_energy: rn(3, 40),
            maximum_energy: rn(3, 60),
            minimum_energy: rn(0, 0),
        }
    }
    fn dc_connector(&self) -> DcConnector {
        DcConnector::Core
    }
    fn now_micros(&self) -> u64 {
        let t = self.0.get();
        self.0.set(t + 1);
        t
    }
}

/// Owned copies of the records we assert on (the real ones borrow).
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Info(std::string::String),
    Error(std::string::String),
    Msg(MsgKind, bool), // (kind, is_tx)
}

#[derive(Default)]
struct Capture(Rc<std::cell::RefCell<std::vec::Vec<Seen>>>);
impl LogSink for Capture {
    fn record(&mut self, r: &Record<'_>) {
        let seen = match (r, r.level()) {
            (Record::MsgTx { msg }, _) => Seen::Msg(*msg, true),
            (Record::MsgRx { msg }, _) => Seen::Msg(*msg, false),
            (_, Level::Info) => Seen::Info(std::format!("{r:?}")),
            (_, Level::Error) => Seen::Error(std::format!("{r:?}")),
            _ => return,
        };
        self.0.borrow_mut().push(seen);
    }
}

struct Rig {
    rx: [u8; 512],
    tx: [u8; 512],
    arena: [u8; 2048],
}

impl Rig {
    fn new() -> Self {
        Self { rx: [0; 512], tx: [0; 512], arena: [0; 2048] }
    }
}

type Sess<'a> = Session<'a, TestEv, Capture>;

fn session<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let Rig { rx, tx, arena } = rig;
    Session::new(Config { poll_interval_ms: 0, allow_unsecured: true, control_pilot: PILOT.with(Cell::get), ..Config::DC_20 }, TestEv::new(), log, Buffers { rx, tx, arena })
}

/// Copies `bytes` into the session's RX buffer and reports them, like a socket read would.
fn deliver(s: &mut Sess<'_>, bytes: &[u8]) -> Step {
    s.rx_space()[..bytes.len()].copy_from_slice(bytes);
    s.handle(Event::Received(bytes.len()))
}

fn framed(payload_type: PayloadType, encode: impl FnOnce(&mut [u8]) -> usize) -> std::vec::Vec<u8> {
    let mut buf = [0u8; 1024];
    let n = encode(&mut buf[HEADER_LEN..]);
    v2gtp::write_header(&mut buf, payload_type, n).unwrap();
    buf[..HEADER_LEN + n].to_vec()
}

const SID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

fn hdr() -> MessageHeader<'static> {
    MessageHeader::new(SID, 5)
}

fn sap_res(code: SapCode, schema_id: Option<u8>) -> std::vec::Vec<u8> {
    framed(PayloadType::SAP, |out| {
        SupportedAppProtocolRes { response_code: code, schema_id }.encode(out).unwrap()
    })
}

fn setup_res(code: ResponseCode, session_id: [u8; 8]) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = SessionSetupRes {
            header: MessageHeader::new(session_id, 5),
            response_code: code,
            evseid: "DE*EVSE*0001",
        };
        msg.encode(out).unwrap()
    })
}

fn auth_setup_res(code: ResponseCode, services: &[Authorization]) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = AuthorizationSetupRes {
            header: hdr(),
            response_code: code,
            authorization_services: services,
            certificate_installation_service: false,
            as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
        };
        msg.encode(out).unwrap()
    })
}

fn auth_res(code: ResponseCode, p: Processing) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        AuthorizationRes { header: hdr(), response_code: code, evse_processing: p }.encode(out).unwrap()
    })
}

/// The framed message the session wants to send, decoded by the common codec.
fn discovery_res(code: ResponseCode, ids: &[u16]) -> std::vec::Vec<u8> {
    let list: std::vec::Vec<Service> = ids.iter().map(|&service_id| Service { service_id, free_service: false }).collect();
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = ServiceDiscoveryRes {
            header: hdr(),
            response_code: code,
            service_renegotiation_supported: false,
            energy_transfer_service_list: ServiceList { service: &list },
            vas_list: None,
        };
        msg.encode(out).unwrap()
    })
}

/// One parameter set per `(id, [(name, int)])` entry.
fn detail_res(code: ResponseCode, service_id: u16, sets: &[(u16, &[(&str, i32)])]) -> std::vec::Vec<u8> {
    let params: std::vec::Vec<std::vec::Vec<Parameter<'_>>> = sets
        .iter()
        .map(|(_, ps)| ps.iter().map(|&(name, v)| Parameter { name, choice: ParameterChoice::IntValue(v) }).collect())
        .collect();
    let sets: std::vec::Vec<ParameterSet<'_>> =
        sets.iter().zip(&params).map(|((id, _), parameters)| ParameterSet { parameter_set_id: *id, parameter: parameters }).collect();
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = ServiceDetailRes {
            header: hdr(),
            response_code: code,
            service_id,
            service_parameter_list: ServiceParameterList { parameter_set: &sets },
        };
        msg.encode(out).unwrap()
    })
}

const DYNAMIC: &[(&str, i32)] = &[("Connector", 1), ("ControlMode", 2), ("MobilityNeedsMode", 1), ("Pricing", 0)];
const SCHEDULED: &[(&str, i32)] = &[("Connector", 1), ("ControlMode", 1), ("MobilityNeedsMode", 1), ("Pricing", 0)];

fn cpd_res(code: ResponseCode) -> std::vec::Vec<u8> {
    framed(PayloadType::DC_MAINSTREAM, |out| {
        let msg = dc::DcChargeParameterDiscoveryRes {
            header: hdr(),
            response_code: code,
            dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Base(dc::DcCpdResEnergyTransferMode {
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_minimum_charge_current: rn(0, 2),
                evse_maximum_voltage: rn(0, 920),
                evse_minimum_voltage: rn(0, 150),
                evse_power_ramp_limitation: None,
            }),
        };
        msg.encode(out).unwrap()
    })
}

fn selection_res(code: ResponseCode) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        ServiceSelectionRes { header: hdr(), response_code: code }.encode(out).unwrap()
    })
}

fn sent(s: &Sess<'_>, step: &Step) -> (PayloadType, std::vec::Vec<u8>) {
    let bytes = &s.tx()[..step.send.expect("nothing to send")];
    let v2gtp::Parse::Frame(f) = v2gtp::parse(bytes, 512).unwrap() else { panic!("incomplete frame") };
    assert_eq!(f.consumed, bytes.len());
    (f.payload_type, f.payload.to_vec())
}

fn sent_common(s: &Sess<'_>, step: &Step, f: impl FnOnce(common::Message<'_>)) {
    let (pt, payload) = sent(s, step);
    assert_eq!(pt, PayloadType::MAINSTREAM);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    f(common::decode(&payload, &arena).unwrap());
}

fn response_timer() -> Timers {
    Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: 2000 })
}

/// Runs the handshake and session setup; returns the session after the
/// `AuthorizationSetupReq` was sent.
fn up_to_authorization_setup<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = session(rig, log);
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(s.state(), State::AwaitAuthorizationSetupRes);
    s
}

/// Up to and including the first `AuthorizationReq`.
fn up_to_authorization<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_authorization_setup(rig, log);
    deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Eim]));
    assert_eq!(s.state(), State::AwaitAuthorizationRes);
    s
}

/// Up to and including the `ServiceDiscoveryReq`.
fn up_to_service_discovery<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_authorization(rig, log);
    deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(s.state(), State::AwaitServiceDiscoveryRes);
    s
}

/// Up to and including the `ServiceDetailReq`.
fn up_to_service_detail<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_service_discovery(rig, log);
    deliver(&mut s, &discovery_res(ResponseCode::Ok, &[2]));
    assert_eq!(s.state(), State::AwaitServiceDetailRes);
    s
}

fn up_to_service_selection<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_service_detail(rig, log);
    deliver(&mut s, &detail_res(ResponseCode::Ok, 2, &[(1, DYNAMIC)]));
    assert_eq!(s.state(), State::AwaitServiceSelectionRes);
    s
}

/// Up to and including the `DC_ChargeParameterDiscoveryReq`.
fn up_to_charge_parameters<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_service_selection(rig, log);
    deliver(&mut s, &selection_res(ResponseCode::Ok));
    assert_eq!(s.state(), State::AwaitChargeParameterDiscoveryRes);
    s
}

/// Up to and including the `ScheduleExchangeReq`.
fn up_to_schedule_exchange<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_charge_parameters(rig, log);
    deliver(&mut s, &cpd_res(ResponseCode::Ok));
    assert_eq!(s.state(), State::AwaitScheduleExchangeRes);
    s
}

fn se_res(code: ResponseCode, p: Processing) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        let msg = common::ScheduleExchangeRes {
            header: hdr(),
            response_code: code,
            evse_processing: p,
            go_to_pause: None,
            se_res_control_mode: common::SeResControlModeChoice::Dynamic(common::DynamicSeResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                schedule: None,
            }),
        };
        msg.encode(out).unwrap()
    })
}

fn cable_res(code: ResponseCode, p: Processing) -> std::vec::Vec<u8> {
    framed(PayloadType::DC_MAINSTREAM, |out| {
        dc::DcCableCheckRes { header: hdr(), response_code: code, evse_processing: p }.encode(out).unwrap()
    })
}

fn precharge_res(code: ResponseCode, volts: i16) -> std::vec::Vec<u8> {
    framed(PayloadType::DC_MAINSTREAM, |out| {
        dc::DcPreChargeRes { header: hdr(), response_code: code, evse_present_voltage: rn(0, volts) }.encode(out).unwrap()
    })
}

fn power_delivery_res(code: ResponseCode) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        common::PowerDeliveryRes { header: hdr(), response_code: code, evse_status: None }.encode(out).unwrap()
    })
}

/// The DC message the session wants to send.
fn sent_dc(s: &Sess<'_>, step: &Step, f: impl FnOnce(dc::Message<'_>)) {
    let (pt, payload) = sent(s, step);
    assert_eq!(pt, PayloadType::DC_MAINSTREAM);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    f(dc::decode(&payload, &arena).unwrap());
}

/// Up to and including the first `DC_CableCheckReq`.
fn up_to_cable_check<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_schedule_exchange(rig, log);
    let step = deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Finished));
    if PILOT.with(Cell::get) {
        // V2G20-917: the pilot is C before the cable check.
        assert_eq!(step.notice, Some(Notice::ControlPilot(ControlPilot::C)));
        assert_eq!(s.state(), State::AwaitControlPilotC);
        assert_eq!(step.send, None, "the cable check waits for the pilot");
        let step = s.handle(Event::ControlPilotChanged);
        assert_eq!(step.notice, Some(Notice::ScheduleExchanged));
    }
    assert_eq!(s.state(), State::AwaitCableCheckRes);
    s
}

/// Up to and including the first `DC_PreChargeReq`.
fn up_to_precharge<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_cable_check(rig, log);
    deliver(&mut s, &cable_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(s.state(), State::AwaitPreChargeRes);
    s
}

fn up_to_contactors<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    INLET.with(|i| i.set(BATTERY_V));
    let mut s = up_to_precharge(rig, log);
    deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V)); // settled: sends the final request
    deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V)); // acknowledged
    assert_eq!(s.state(), State::AwaitContactors);
    s
}

/// Up to and including the first `DC_ChargeLoopReq`.
fn up_to_charge_loop<'a>(rig: &'a mut Rig, log: Capture) -> Sess<'a> {
    let mut s = up_to_contactors(rig, log);
    s.handle(Event::ContactorsClosed);
    deliver(&mut s, &power_delivery_res(ResponseCode::Ok));
    assert_eq!(s.state(), State::AwaitChargeLoopRes);
    s
}

fn loop_res(notification: Option<iso15118_20::common_types::EvseNotification>) -> std::vec::Vec<u8> {
    framed(PayloadType::DC_MAINSTREAM, |out| {
        let msg = dc::DcChargeLoopRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            evse_status: notification.map(|n| iso15118_20::common_types::EvseStatus { notification_max_delay: 7, evse_notification: n }),
            meter_info: None,
            receipt: None,
            evse_present_current: rn(0, 100),
            evse_present_voltage: rn(0, 398),
            evse_power_limit_achieved: false,
            evse_current_limit_achieved: true,
            evse_voltage_limit_achieved: false,
            cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 100),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
            }),
        };
        msg.encode(out).unwrap()
    })
}

fn session_stop_res(code: ResponseCode) -> std::vec::Vec<u8> {
    framed(PayloadType::MAINSTREAM, |out| {
        common::SessionStopRes { header: hdr(), response_code: code }.encode(out).unwrap()
    })
}

/// The vehicle ends the session for a reason of its own before the power is on (V2G20-2644): it says so with
/// `SessionStopReq` (Terminate, the reason as the termination code), and ends with the reason once the answer is in
/// (or does not come).
fn ends_after_a_session_stop(s: &mut Sess<'_>, step: &Step, code: &str, reason: EndReason) {
    assert_eq!(step.end, None);
    sent_common(s, step, |m| {
        let common::Message::SessionStopReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.charging_session, common::ChargingSession::Terminate);
        assert_eq!(r.ev_termination_code, Some(code));
    });
    assert_eq!(s.state(), State::AwaitSessionStopRes);
    // The answer: the end with the reason of the vehicle.
    assert_eq!(deliver(s, &session_stop_res(ResponseCode::Ok)).end, Some(reason));
}

fn loop_timer() -> Timers {
    Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: 500 })
}

/// The shutdown after a stop trigger: ramp down, PowerDeliveryReq(Stop), SessionStopReq.
fn finish_shutdown(s: &mut Sess<'_>) {
    let step = s.handle(Event::EnergyTransferStopped);
    sent_common(s, &step, |m| {
        let common::Message::PowerDeliveryReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.charge_progress, common::ChargeProgress::Stop);
        assert!(r.ev_power_profile.is_some(), "V2G20-1546: a profile accompanies Stop");
    });
    assert_eq!(s.state(), State::AwaitPowerDeliveryStopRes);
    let step = deliver(s, &power_delivery_res(ResponseCode::Ok));
    assert_eq!(step.notice, Some(Notice::OpenContactors));
    assert_eq!(step.send, None, "welding detection waits for the open contactors");
    assert_eq!(step.timers, Timers::one(TimerCmd::Cancel(Timer::Response)).with(sequence_timer()));
    // V2G20-1448: after the stop the EVCC performs welding detection ...
    let mut step = s.handle(Event::ContactorsOpened);
    if PILOT.with(Cell::get) {
        // ... with the pilot at B (V2G20-919).
        assert_eq!(step.notice, Some(Notice::ControlPilot(ControlPilot::B)));
        assert_eq!(s.state(), State::AwaitControlPilotB);
        step = s.handle(Event::ControlPilotChanged);
    }
    sent_dc(s, &step, |m| {
        let dc::Message::DcWeldingDetectionReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Ongoing);
    });
    assert_eq!(s.state(), State::AwaitWeldingDetectionRes);
    // ... repeating while the EVSE reports a voltage on its side B (V2G20-1573) ...
    // (the EVSE's side B still carries 150 V: the criterion is the reported voltage)
    let step = deliver(s, &welding_res(ResponseCode::Ok, 150));
    sent_dc(s, &step, |m| {
        let dc::Message::DcWeldingDetectionReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Ongoing);
    });
    // ... until it is finished (V2G20-1461), and only then stops the session (V2G20-1462).
    let step = deliver(s, &welding_res(ResponseCode::Ok, 3));
    sent_dc(s, &step, |m| {
        let dc::Message::DcWeldingDetectionReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Finished);
    });
    let step = deliver(s, &welding_res(ResponseCode::Ok, 3));
    sent_common(s, &step, |m| {
        let common::Message::SessionStopReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.charging_session, common::ChargingSession::Terminate);
    });
    let step = deliver(s, &session_stop_res(ResponseCode::Ok));
    assert_eq!(step.end, Some(EndReason::Completed));
}

fn sequence_timer() -> TimerCmd {
    TimerCmd::Arm { timer: Timer::Sequence, ms: 40_000 }
}

fn welding_res(code: ResponseCode, volts: i16) -> std::vec::Vec<u8> {
    framed(PayloadType::DC_MAINSTREAM, |out| {
        dc::DcWeldingDetectionRes { header: hdr(), response_code: code, evse_present_voltage: rn(0, volts) }.encode(out).unwrap()
    })
}

#[test]
fn the_charge_loop_cycles_and_stops_on_request() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    // A response: the loop interval (100 ms) passes, then the next request goes out (0.5 s timeout).
    for _ in 0..3 {
        let step = deliver(&mut s, &loop_res(None));
        assert_eq!(step.send, None);
        assert_eq!(
            step.timers,
            Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Poll, ms: 100 })
        );
        let Some(Notice::ChargeLoop(status)) = step.notice else { panic!("{step:?}") };
        assert_eq!(status.present_current, rn(0, 100));
        assert!(status.current_limit_achieved);
        assert_eq!(status.maximum_charge_current, rn(0, 300));
        let step = s.handle(Event::TimerExpired(Timer::Poll));
        assert_eq!(step.timers, loop_timer());
        sent_dc(&s, &step, |m| {
            let dc::Message::DcChargeLoopReq(r) = m else { panic!("{m:?}") };
            assert_eq!(r.ev_present_voltage, rn(0, BATTERY_V));
            let dc::ClReqControlModeChoice::DynamicDc(mode) = r.cl_req_control_mode else { panic!() };
            assert_eq!(mode.ev_maximum_charge_current, rn(0, 200));
        });
    }
    // The vehicle wants to stop while the request is outstanding: acted on at the response.
    assert_eq!(s.handle(Event::StopRequested), Step::default());
    let step = deliver(&mut s, &loop_res(None));
    assert_eq!(step.notice, Some(Notice::StopEnergyTransfer { within_s: None }));
    assert_eq!(s.state(), State::AwaitCurrentStopped);
    finish_shutdown(&mut s);
}

/// The discovery announces what the vehicle can ever take; the loop carries what it can take now,
/// within the announced envelope.
#[test]
fn the_charge_loop_carries_the_present_limits_not_the_announced_ones() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    for (now, sent) in [(80, 80), (250, 200), (0, 200)] {
        LOOP_CURRENT.with(|c| c.set(now));
        deliver(&mut s, &loop_res(None));
        let step = s.handle(Event::TimerExpired(Timer::Poll));
        sent_dc(&s, &step, |m| {
            let dc::Message::DcChargeLoopReq(r) = m else { panic!("{m:?}") };
            let dc::ClReqControlModeChoice::DynamicDc(mode) = r.cl_req_control_mode else { panic!() };
            assert_eq!(mode.ev_maximum_charge_current, rn(0, sent), "limit {now}");
        });
    }
    LOOP_CURRENT.with(|c| c.set(0));
}

#[test]
fn a_stop_request_during_the_loop_interval_acts_at_once() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(None)); // now waiting for the poll timer
    let step = s.handle(Event::StopRequested);
    assert_eq!(step.notice, Some(Notice::StopEnergyTransfer { within_s: None }));
    assert_eq!(
        step.timers,
        Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Poll)).with(sequence_timer())
    );
    // The poll timer may already have fired: it must not resume the loop.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Poll)), Step::default());
    finish_shutdown(&mut s);
}

#[test]
fn a_voltage_that_does_not_fall_during_welding_detection_means_a_welded_contactor() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(None));
    s.handle(Event::StopRequested);
    s.handle(Event::EnergyTransferStopped);
    deliver(&mut s, &power_delivery_res(ResponseCode::Ok));
    s.handle(Event::ContactorsOpened);
    for _ in 0..3 {
        let step = deliver(&mut s, &welding_res(ResponseCode::Ok, 400));
        assert_eq!(step.end, None);
    }
    let step = s.handle(Event::TimerExpired(Timer::Ongoing));
    assert_eq!(step.end, Some(EndReason::ContactorWelded));
}

#[test]
fn the_evse_can_terminate_the_charge() {
    // V2G20-1416, V2G20-1960
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    let step = deliver(&mut s, &loop_res(Some(iso15118_20::common_types::EvseNotification::Terminate)));
    assert_eq!(step.notice, Some(Notice::StopEnergyTransfer { within_s: Some(7) }));
    finish_shutdown(&mut s);

    // Other notifications are not handled yet: the loop just goes on.
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    let step = deliver(&mut s, &loop_res(Some(iso15118_20::common_types::EvseNotification::MeteringConfirmation)));
    assert!(matches!(step.notice, Some(Notice::ChargeLoop(_))));
    assert_eq!(s.state(), State::AwaitChargeLoopRes);
}

#[test]
fn the_charge_loop_has_a_half_second_timeout_and_no_ongoing_bound() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)), Step::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
}

#[test]
fn stop_events_are_ignored_when_they_do_not_apply() {
    let mut rig = Rig::new();
    let mut s = up_to_precharge(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::StopRequested), Step::default());
    assert_eq!(s.handle(Event::EnergyTransferStopped), Step::default());
    assert_eq!(s.state(), State::AwaitPreChargeRes);
}

#[test]
fn cable_check_pre_charge_and_power_delivery() {
    let mut rig = Rig::new();
    let mut s = up_to_schedule_exchange(&mut rig, Capture::default());

    // ScheduleExchange done: the cable check starts and its 40 s timer with it (V2G20-700).
    let step = deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::ScheduleExchanged));
    assert_eq!(step.timers, response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 40_000 }));
    sent_dc(&s, &step, |m| assert!(matches!(m, dc::Message::DcCableCheckReq(_))));

    // Cable check ongoing: repeated, the 40 s timer is not touched again.
    let step = deliver(&mut s, &cable_res(ResponseCode::Ok, Processing::Ongoing));
    assert_eq!(step.timers, response_timer());
    sent_dc(&s, &step, |m| assert!(matches!(m, dc::Message::DcCableCheckReq(_))));

    // Finished: pre-charge starts with its own 10 s timer (V2G20-704), EV voltage = battery.
    let step = deliver(&mut s, &cable_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::CableChecked));
    assert_eq!(step.timers, response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 10_000 }));
    sent_dc(&s, &step, |m| {
        let dc::Message::DcPreChargeReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Ongoing);
        assert_eq!(r.ev_target_voltage, rn(0, BATTERY_V));
    });

    // The inlet voltage is still far from the battery: ask again.
    INLET.with(|i| i.set(300));
    let step = deliver(&mut s, &precharge_res(ResponseCode::Ok, 300));
    sent_dc(&s, &step, |m| {
        let dc::Message::DcPreChargeReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Ongoing);
    });

    // Within 20 V (IEC 61851-23): the EV says it is finished ...
    INLET.with(|i| i.set(BATTERY_V - 19));
    let step = deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V - 19));
    sent_dc(&s, &step, |m| {
        let dc::Message::DcPreChargeReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.ev_processing, Processing::Finished);
    });
    // ... and when that is acknowledged the host is asked to close the contactors.
    let step = deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V - 19));
    assert_eq!(step.notice, Some(Notice::CloseContactors));
    assert_eq!(step.send, None);
    // The vehicle now has 40 s (V2G_EVCC_Sequence_Performance_Time) to close its contactors.
    assert_eq!(
        step.timers,
        Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Ongoing)).with(sequence_timer())
    );
    assert_eq!(s.state(), State::AwaitContactors);

    // Only then does the EV request power (V2G20-1546: with a power profile).
    let step = s.handle(Event::ContactorsClosed);
    assert_eq!(step.timers, response_timer().with(TimerCmd::Cancel(Timer::Sequence)));
    sent_common(&s, &step, |m| {
        let common::Message::PowerDeliveryReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.charge_progress, common::ChargeProgress::Start);
        assert_eq!(r.ev_processing, Processing::Finished);
        let profile = r.ev_power_profile.expect("profile accompanies Start");
        assert_eq!(profile.ev_power_profile_entries.ev_power_profile_entry[0].power, rn(3, 100));
    });
    let step = deliver(&mut s, &power_delivery_res(ResponseCode::Ok));
    assert_eq!(step.notice, Some(Notice::EnergyTransferStarted));
    // Power is on: the charge loop starts at once, with its half second timeout (Table 217).
    assert_eq!(step.timers, loop_timer());
    sent_dc(&s, &step, |m| assert!(matches!(m, dc::Message::DcChargeLoopReq(_))));
    assert_eq!(s.state(), State::AwaitChargeLoopRes);
}

#[test]
fn contactors_are_only_closed_when_asked() {
    let mut rig = Rig::new();
    let mut s = up_to_precharge(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::ContactorsClosed), Step::default());
    assert_eq!(s.state(), State::AwaitPreChargeRes);
}

#[test]
fn cable_check_and_pre_charge_have_their_own_timeouts() {
    let mut rig = Rig::new();
    let mut s = up_to_cable_check(&mut rig, Capture::default());
    deliver(&mut s, &cable_res(ResponseCode::Ok, Processing::Ongoing));
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)).end, Some(EndReason::Timeout));

    let mut rig = Rig::new();
    let mut s = up_to_precharge(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)).end, Some(EndReason::Timeout));
    let mut rig = Rig::new();
    let mut s = up_to_precharge(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
}

#[test]
fn dc_failures_end_the_session() {
    let mut rig = Rig::new();
    let mut s = up_to_cable_check(&mut rig, Capture::default());
    let step = deliver(&mut s, &cable_res(ResponseCode::Failed, Processing::Finished));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::Failed)));

    let mut rig = Rig::new();
    let mut s = up_to_precharge(&mut rig, Capture::default());
    let step = deliver(&mut s, &precharge_res(ResponseCode::FailedSequenceError, 0));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));

    let mut rig = Rig::new();
    let mut s = up_to_contactors(&mut rig, Capture::default());
    s.handle(Event::ContactorsClosed);
    let step = deliver(&mut s, &power_delivery_res(ResponseCode::FailedPowerDeliveryNotApplied));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedPowerDeliveryNotApplied)));
}

/// With a poll interval the repeat waits for the poll timer instead of going out at once.
#[test]
fn ongoing_answers_are_polled_at_the_configured_pace() {
    let mut rig = Rig::new();
    let Rig { rx, tx, arena } = &mut rig;
    let mut s = Session::new(Config { allow_unsecured: true, ..Config::DC_20 }, TestEv::new(), Capture::default(), Buffers { rx, tx, arena });
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Eim]));

    let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Ongoing));
    assert_eq!(step.send, None, "the repeat waits");
    assert_eq!(
        step.timers,
        Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Poll, ms: 200 })
    );
    // When the pause is over the request goes out, with the response and ongoing timers.
    let step = s.handle(Event::TimerExpired(Timer::Poll));
    assert_eq!(step.timers, response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 60_000 }));
    sent_common(&s, &step, |m| assert!(matches!(m, common::Message::AuthorizationReq(_))));
    // A stray poll expiry does nothing.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Poll)), Step::default());
}

#[test]
fn schedule_exchange_repeats_while_ongoing_then_finishes() {
    let mut rig = Rig::new();
    let mut s = up_to_schedule_exchange(&mut rig, Capture::default());
    let mut stamps = std::vec::Vec::new();
    for i in 0..3 {
        let step = deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Ongoing));
        // The ongoing bound is armed once, with the first Ongoing answer.
        let want = if i == 0 { response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 60_000 }) } else { response_timer() };
        assert_eq!(step.timers, want);
        sent_common(&s, &step, |m| {
            let common::Message::ScheduleExchangeReq(req) = m else { panic!("{m:?}") };
            assert_eq!(req.maximum_supporting_points, 12);
            stamps.push(req.header.time_stamp);
        });
    }
    assert!(stamps.windows(2).all(|w| w[0] < w[1]));
    let step = deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::ScheduleExchanged));
    // The cable check follows at once, and its own bound replaces the ongoing timer.
    assert_eq!(step.timers, response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 40_000 }));
    assert_eq!(s.state(), State::AwaitCableCheckRes);
}

#[test]
fn schedule_exchange_failures_and_timeouts() {
    let mut rig = Rig::new();
    let mut s = up_to_schedule_exchange(&mut rig, Capture::default());
    let step = deliver(&mut s, &se_res(ResponseCode::FailedScheduleSelectionInvalid, Processing::Finished));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedScheduleSelectionInvalid)));

    let mut rig = Rig::new();
    let mut s = up_to_schedule_exchange(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));

    let mut rig = Rig::new();
    let mut s = up_to_schedule_exchange(&mut rig, Capture::default());
    deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Ongoing));
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)).end, Some(EndReason::Timeout));
}

#[test]
fn service_negotiation_failures_end_the_session() {
    // The EVSE does not offer DC.
    let mut rig = Rig::new();
    let mut s = up_to_service_discovery(&mut rig, Capture::default());
    let step = deliver(&mut s, &discovery_res(ResponseCode::Ok, &[1]));
    ends_after_a_session_stop(&mut s, &step, "NoCompatibleService", EndReason::NoCompatibleService);

    let mut rig = Rig::new();
    let mut s = up_to_service_discovery(&mut rig, Capture::default());
    let step = deliver(&mut s, &discovery_res(ResponseCode::FailedSequenceError, &[2]));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));

    // Only a scheduled parameter set: this EV does Dynamic.
    let mut rig = Rig::new();
    let mut s = up_to_service_detail(&mut rig, Capture::default());
    let step = deliver(&mut s, &detail_res(ResponseCode::Ok, 2, &[(1, SCHEDULED)]));
    ends_after_a_session_stop(&mut s, &step, "NoCompatibleParameterSet", EndReason::NoCompatibleParameterSet);

    let mut rig = Rig::new();
    let mut s = up_to_service_detail(&mut rig, Capture::default());
    let step = deliver(&mut s, &detail_res(ResponseCode::FailedServiceIdInvalid, 2, &[(1, DYNAMIC)]));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedServiceIdInvalid)));

    // A detail for another service than the one asked about.
    let mut rig = Rig::new();
    let mut s = up_to_service_detail(&mut rig, Capture::default());
    let step = deliver(&mut s, &detail_res(ResponseCode::Ok, 1, &[(1, DYNAMIC)]));
    assert_eq!(step.end, Some(EndReason::UnexpectedMessage));

    let mut rig = Rig::new();
    let mut s = up_to_service_selection(&mut rig, Capture::default());
    let step = deliver(&mut s, &selection_res(ResponseCode::FailedServiceSelectionInvalid));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedServiceSelectionInvalid)));

    let mut rig = Rig::new();
    let mut s = up_to_charge_parameters(&mut rig, Capture::default());
    let step = deliver(&mut s, &cpd_res(ResponseCode::FailedWrongChargeParameter));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedWrongChargeParameter)));

    // A DC message on the common payload type is not what the state expects.
    let mut rig = Rig::new();
    let mut s = up_to_charge_parameters(&mut rig, Capture::default());
    let mut wrong = cpd_res(ResponseCode::Ok);
    wrong[2..4].copy_from_slice(&PayloadType::MAINSTREAM.0.to_be_bytes());
    assert_eq!(deliver(&mut s, &wrong), Step::default());
}

#[test]
fn every_service_step_has_a_response_timeout() {
    let mut rig = Rig::new();
    let mut s = up_to_service_discovery(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
    let mut rig = Rig::new();
    let mut s = up_to_service_detail(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
    let mut rig = Rig::new();
    let mut s = up_to_service_selection(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
    let mut rig = Rig::new();
    let mut s = up_to_charge_parameters(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
}

#[test]
fn full_flow_to_authorized() {
    let log = Capture::default();
    let seen = log.0.clone();
    let mut rig = Rig::new();
    let mut s = session(&mut rig, log);

    let step = s.handle(Event::Start { secured: true });
    // The 20 s communication setup timer runs until the SessionSetupRes (V2G20-2298).
    assert_eq!(step.timers, response_timer().with(TimerCmd::Arm { timer: Timer::Setup, ms: 20_000 }));
    let (pt, payload) = sent(&s, &step);
    assert_eq!(pt, PayloadType::SAP);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    let app_protocol::Message::SupportedAppProtocolReq(req) = app_protocol::decode(&payload, &arena).unwrap() else { panic!() };
    assert_eq!(req.app_protocol, &[Config::DC_20.offer]);

    let step = deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    assert_eq!(step.timers, response_timer());
    sent_common(&s, &step, |m| {
        let common::Message::SessionSetupReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.evccid, "WMIV1234567890ABCDEX");
        assert_eq!(req.header.session_id, [0; 8], "a new session starts with an all-zero id");
    });

    // SessionSetupRes: the EV learns the session id and moves on to AuthorizationSetupReq.
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.notice, Some(Notice::SessionSetUp { session_id: SID }));
    assert_eq!(step.timers, response_timer().with(TimerCmd::Cancel(Timer::Setup)));
    sent_common(&s, &step, |m| {
        let common::Message::AuthorizationSetupReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.header.session_id, SID, "later messages carry the id the EVSE assigned");
    });

    let step = deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::PnC, Authorization::Eim]));
    sent_common(&s, &step, |m| {
        let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.header.session_id, SID);
    });

    // Authorization done: the EV asks which services exist, and the ongoing timer is cancelled.
    let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::Authorized));
    assert_eq!(step.timers, response_timer().with(TimerCmd::Cancel(Timer::Ongoing)));
    assert_eq!(s.state(), State::AwaitServiceDiscoveryRes);
    sent_common(&s, &step, |m| {
        let common::Message::ServiceDiscoveryReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.header.session_id, SID);
        assert!(req.supported_service_ids.is_none(), "ask for every service");
    });

    let step = deliver(&mut s, &discovery_res(ResponseCode::Ok, &[1, 2]));
    // ServiceDetailReq has a longer response timeout: 5 s (Table 215).
    assert_eq!(step.timers, Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: 5000 }));
    sent_common(&s, &step, |m| {
        let common::Message::ServiceDetailReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.service_id, 2);
    });

    // Two parameter sets: the scheduled one is skipped, the dynamic one (id 7) is chosen.
    let step = deliver(&mut s, &detail_res(ResponseCode::Ok, 2, &[(3, SCHEDULED), (7, DYNAMIC)]));
    assert_eq!(step.timers, response_timer());
    sent_common(&s, &step, |m| {
        let common::Message::ServiceSelectionReq(req) = m else { panic!("{m:?}") };
        assert_eq!(req.selected_energy_transfer_service, SelectedService { service_id: 2, parameter_set_id: 7 });
        assert!(req.selected_vas_list.is_none());
    });

    let step = deliver(&mut s, &selection_res(ResponseCode::Ok));
    assert_eq!(step.notice, Some(Notice::ServiceSelected { service_id: 2, parameter_set_id: 7 }));
    assert_eq!(step.timers, response_timer());
    assert_eq!(s.state(), State::AwaitChargeParameterDiscoveryRes);
    // The EV's DC limits come from the vehicle, on the DC payload type.
    let (pt, payload) = sent(&s, &step);
    assert_eq!(pt, PayloadType::DC_MAINSTREAM);
    let mut mem = [0u8; 1024];
    let arena = Arena::new(&mut mem);
    let dc::Message::DcChargeParameterDiscoveryReq(req) = dc::decode(&payload, &arena).unwrap() else { panic!() };
    let dc::DcCpdReqEnergyTransferModeChoice::Base(mode) = req.dc_cpd_req_energy_transfer_mode else { panic!() };
    assert_eq!(mode.ev_maximum_voltage, rn(0, 450));
    assert_eq!(mode.target_soc, Some(80));
    assert_eq!(req.header.session_id, SID);

    let step = deliver(&mut s, &cpd_res(ResponseCode::Ok));
    let Some(Notice::ChargeParametersDiscovered(evse)) = step.notice else { panic!("{step:?}") };
    let dc::DcCpdResEnergyTransferModeChoice::Base(mode) = evse else { panic!() };
    assert_eq!(mode.evse_maximum_voltage, rn(0, 920));
    assert_eq!(step.timers, response_timer());
    assert_eq!(s.state(), State::AwaitScheduleExchangeRes);
    sent_common(&s, &step, |m| {
        let common::Message::ScheduleExchangeReq(req) = m else { panic!("{m:?}") };
        let common::SeReqControlModeChoice::Dynamic(mode) = req.se_req_control_mode else { panic!() };
        assert_eq!(mode.departure_time, 3600);
        assert_eq!(mode.target_soc, Some(80));
        assert_eq!(mode.ev_maximum_energy_request, rn(3, 60));
    });

    let step = deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::ScheduleExchanged));
    assert_eq!(s.state(), State::AwaitCableCheckRes);

    let seen = seen.borrow();
    for (kind, tx) in [
        (MsgKind::SupportedAppProtocolReq, true),
        (MsgKind::SessionSetupRes, false),
        (MsgKind::AuthorizationSetupReq, true),
        (MsgKind::AuthorizationRes, false),
        (MsgKind::ServiceDiscoveryReq, true),
        (MsgKind::ServiceDetailRes, false),
        (MsgKind::ServiceSelectionReq, true),
        (MsgKind::ServiceSelectionRes, false),
        (MsgKind::DcChargeParameterDiscoveryReq, true),
        (MsgKind::DcChargeParameterDiscoveryRes, false),
        (MsgKind::ScheduleExchangeReq, true),
        (MsgKind::ScheduleExchangeRes, false),
    ] {
        assert!(seen.contains(&Seen::Msg(kind, tx)), "missing {kind:?} tx={tx}");
    }
    assert!(!seen.iter().any(|s| matches!(s, Seen::Error(_))), "{seen:?}");
}

/// Ref: ISO 15118-20:2022 V2G20-1582: while EVSEProcessing is Ongoing the
/// EVCC repeats the AuthorizationReq, unaltered except for the timestamp.
#[test]
fn ongoing_authorization_repeats_the_request_with_a_new_timestamp() {
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let first_ts = {
        // Re-trigger so we can look at a request: the first one was already sent.
        let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Ongoing));
        let mut ts = 0;
        sent_common(&s, &step, |m| {
            let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
            assert_eq!(req.header.session_id, SID);
            ts = req.header.time_stamp;
        });
        // First Ongoing: arms the response timer and, once, the ongoing timer.
        assert_eq!(
            step.timers,
            response_timer().with(TimerCmd::Arm { timer: Timer::Ongoing, ms: 60_000 })
        );
        ts
    };
    for processing in [Processing::Ongoing, Processing::OngoingWaitingForCustomerInteraction] {
        let step = deliver(&mut s, &auth_res(ResponseCode::Ok, processing));
        assert_eq!(s.state(), State::AwaitAuthorizationRes);
        assert_eq!(step.timers, response_timer(), "the ongoing timer must not be re-armed");
        let mut ts = 0;
        sent_common(&s, &step, |m| {
            let common::Message::AuthorizationReq(req) = m else { panic!("{m:?}") };
            ts = req.header.time_stamp;
        });
        assert!(ts > first_ts);
    }
    let step = deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    assert_eq!(step.notice, Some(Notice::Authorized));
}

#[test]
fn ongoing_forever_hits_the_ongoing_timeout() {
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    for _ in 0..10 {
        deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Ongoing));
    }
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)).end, Some(EndReason::Timeout));
}

#[test]
fn stale_timers_are_ignored() {
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(Some(iso15118_20::common_types::EvseNotification::Terminate)));
    assert_eq!(s.state(), State::AwaitCurrentStopped);
    // All timers were cancelled, but an expiry may already have been in flight.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)), Step::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)), Step::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Poll)), Step::default());
    assert_eq!(s.state(), State::AwaitCurrentStopped);

    // The ongoing timer is cancelled once authorization finishes, too.
    let mut rig = Rig::new();
    let mut s = up_to_service_discovery(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Ongoing)), Step::default());
    assert_eq!(s.state(), State::AwaitServiceDiscoveryRes);
}

#[test]
fn authorization_failures_end_the_session() {
    // EVSE offers only PnC, which this EV does not implement.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::PnC]));
    ends_after_a_session_stop(&mut s, &step, "NoCompatibleAuthorization", EndReason::NoCompatibleAuthorization);
    // Without an answer the vehicle ends with the same reason.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::PnC]));
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::NoCompatibleAuthorization));

    // AuthorizationSetupRes with a failing code.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_setup_res(ResponseCode::FailedSequenceError, &[Authorization::Eim]));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));

    // AuthorizationRes: the EVSE refuses the user.
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    let step = deliver(&mut s, &auth_res(ResponseCode::WarningEimAuthorizationFailure, Processing::Finished));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::WarningEimAuthorizationFailure)));
}

#[test]
fn frames_split_across_reads_are_reassembled() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let res = sap_res(SapCode::OkSuccessfulNegotiation, Some(1));
    for byte in &res[..res.len() - 1] {
        assert_eq!(deliver(&mut s, &[*byte]), Step::default());
    }
    let step = deliver(&mut s, &res[res.len() - 1..]);
    assert!(step.send.is_some());
}

#[test]
fn timeout_ends_the_session() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let step = s.handle(Event::TimerExpired(Timer::Response));
    assert_eq!(step.end, Some(EndReason::Timeout));
    assert_eq!(s.state(), State::Ended);
    // Once ended, nothing else happens.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)), Step::default());
    assert_eq!(deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1))), Step::default());
}

#[test]
fn rejected_protocol_ends_with_no_common_protocol() {
    for (code, id) in [(SapCode::FailedNoNegotiation, None), (SapCode::OkSuccessfulNegotiation, Some(9)), (SapCode::OkSuccessfulNegotiation, None)] {
        let mut rig = Rig::new();
        let mut s = session(&mut rig, Capture::default());
        s.handle(Event::Start { secured: false });
        assert_eq!(deliver(&mut s, &sap_res(code, id)).end, Some(EndReason::NoCommonProtocol));
    }
}

#[test]
fn failing_session_setup_code_is_reported() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiationWithMinorDeviation, Some(1)));
    let step = deliver(&mut s, &setup_res(ResponseCode::FailedSequenceError, SID));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::FailedSequenceError)));
}

/// Ref: ISO 15118-20:2022 V2G20-440: anything that is not a valid response to the request in
/// flight is ignored, and the response timer keeps running.
#[test]
fn messages_that_are_not_the_awaited_response_are_ignored() {
    // SessionSetupRes arriving while SupportedAppProtocolRes is expected.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    assert_eq!(deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID)), Step::default());
    assert_eq!(s.state(), State::AwaitSupportedAppProtocolRes);
    // The real answer is still accepted afterwards.
    let step = deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    assert_eq!(s.state(), State::AwaitSessionSetupRes);
    assert!(step.send.is_some());

    // A different common message than the one awaited.
    let mut rig = Rig::new();
    let mut s = up_to_authorization(&mut rig, Capture::default());
    assert_eq!(deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID)), Step::default());
    // The right message under the wrong payload type.
    let wrong = framed(PayloadType::DC_MAINSTREAM, |out| {
        AuthorizationRes { header: hdr(), response_code: ResponseCode::Ok, evse_processing: Processing::Finished }.encode(out).unwrap()
    });
    assert_eq!(deliver(&mut s, &wrong), Step::default());
    assert_eq!(s.state(), State::AwaitAuthorizationRes);
    // Still waiting: the timeout ends it.
    assert_eq!(s.handle(Event::TimerExpired(Timer::Response)).end, Some(EndReason::Timeout));
}

#[test]
fn the_communication_setup_timer_bounds_the_start_of_the_session() {
    // V2G20-2298, V2G20-448: no SessionSetupRes within 20 s ends the session.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    assert_eq!(s.handle(Event::TimerExpired(Timer::Setup)).end, Some(EndReason::Timeout));

    // After SessionSetupRes the timer is cancelled and a late expiry is stale.
    let mut rig = Rig::new();
    let mut s = up_to_authorization_setup(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Setup)), Step::default());
}

#[test]
fn iso_15118_20_is_not_offered_without_tls() {
    // V2G20-1235, V2G20-1237
    let mut rig = Rig::new();
    let Rig { rx, tx, arena } = &mut rig;
    let mut s = Session::new(Config::DC_20, TestEv::new(), Capture::default(), Buffers { rx, tx, arena });
    let step = s.handle(Event::Start { secured: false });
    assert_eq!(step.end, Some(EndReason::TlsRequired));
    assert_eq!(step.send, None);
}

#[test]
fn power_tolerance_warning_does_not_stop_the_start_of_the_energy_transfer() {
    // V2G20-1438
    let mut rig = Rig::new();
    let mut s = up_to_contactors(&mut rig, Capture::default());
    s.handle(Event::ContactorsClosed);
    let step = deliver(&mut s, &power_delivery_res(ResponseCode::WarningPowerToleranceNotConfirmed));
    assert_eq!(step.notice, Some(Notice::EnergyTransferStarted));
    assert_eq!(s.state(), State::AwaitChargeLoopRes);
    // Other warnings are not tolerated.
    let mut rig = Rig::new();
    let mut s = up_to_contactors(&mut rig, Capture::default());
    s.handle(Event::ContactorsClosed);
    let step = deliver(&mut s, &power_delivery_res(ResponseCode::WarningStandbyNotAllowed));
    assert_eq!(step.end, Some(EndReason::Rejected(ResponseCode::WarningStandbyNotAllowed)));
}

#[test]
fn the_vehicle_has_a_bounded_time_for_its_own_actions() {
    // V2G_EVCC_Sequence_Performance_Time: closing contactors, ramp-down, opening contactors.
    let mut rig = Rig::new();
    let mut s = up_to_contactors(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::TimerExpired(Timer::Sequence)).end, Some(EndReason::Timeout));

    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(Some(iso15118_20::common_types::EvseNotification::Terminate)));
    assert_eq!(s.handle(Event::TimerExpired(Timer::Sequence)).end, Some(EndReason::Timeout));
}

#[test]
fn garbage_is_rejected_without_panicking() {
    // Bad V2GTP version.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    assert_eq!(deliver(&mut s, &[0x02, 0xFD, 0x80, 0x01, 0, 0, 0, 0]).end, Some(EndReason::Framing));

    // Valid framing, undecodable payload.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    let bad = framed(PayloadType::SAP, |out| {
        out[..3].copy_from_slice(&[0x80, 0xff, 0xff]);
        3
    });
    assert_eq!(deliver(&mut s, &bad), Step::default(), "an undecodable payload is ignored (V2G20-440)");
    assert_eq!(s.state(), State::AwaitSupportedAppProtocolRes);

    // A frame bigger than the RX buffer.
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    s.handle(Event::Start { secured: false });
    assert_eq!(deliver(&mut s, &[0x01, 0xFE, 0x80, 0x01, 0, 0, 0xff, 0xff]).end, Some(EndReason::Framing));
}

#[test]
fn truncated_responses_never_panic_and_never_complete() {
    let full = auth_res(ResponseCode::Ok, Processing::Finished);
    for cut in 0..full.len() {
        let mut rig = Rig::new();
        let mut s = up_to_authorization(&mut rig, Capture::default());
        let step = deliver(&mut s, &full[..cut]);
        assert_eq!(step, Step::default(), "cut {cut} produced output");
        assert_eq!(s.state(), State::AwaitAuthorizationRes);
    }
}

#[test]
fn tiny_tx_buffer_ends_the_session() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 12];
    let mut arena = [0u8; 512];
    let mut s = Session::new(Config { allow_unsecured: true, ..Config::DC_20 }, TestEv::new(), Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    assert_eq!(s.handle(Event::Start { secured: false }).end, Some(EndReason::TxBufferTooSmall));
}

#[test]
fn small_arena_is_a_decode_error_not_a_panic() {
    let mut rx = [0u8; 256];
    let mut tx = [0u8; 256];
    let mut arena = [0u8; 8];
    let mut s = Session::new(Config { allow_unsecured: true, ..Config::DC_20 }, TestEv::new(), Capture::default(), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    assert_eq!(step.end, Some(EndReason::Malformed(DecodeError::ArenaFull)));
}

/// Ref: ISO 15118-20:2022 V2G20-1534..1537: header time stamps are in EVSE time (microseconds),
/// learned from the SessionSetupRes, and increase with every message.
#[test]
fn time_stamps_follow_the_evse_clock_and_increase() {
    let mut rig = Rig::new();
    let mut s = session(&mut rig, Capture::default());
    let step = s.handle(Event::Start { secured: false });
    let _ = step;
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    let first = {
        let step = deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
        let mut ts = 0;
        sent_common(&s, &step, |m| {
            let common::Message::AuthorizationSetupReq(r) = m else { panic!("{m:?}") };
            ts = r.header.time_stamp;
        });
        ts
    };
    // The EVSE said 5 in SessionSetupRes (setup_res uses time stamp 5); the vehicle's own clock
    // reads about 1.7e9, so only the learned offset can explain such small values.
    assert!((5..1000).contains(&first), "{first}");
    let step = deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Eim]));
    let mut second = 0;
    sent_common(&s, &step, |m| {
        let common::Message::AuthorizationReq(r) = m else { panic!("{m:?}") };
        second = r.header.time_stamp;
    });
    assert!(second > first && (5..1000).contains(&second), "{second}");
}

#[test]
fn a_bpt_session_uses_the_bpt_variants() {
    let mut rig = Rig::new();
    let Rig { rx, tx, arena } = &mut rig;
    let config = Config { poll_interval_ms: 0, allow_unsecured: true, ..Config::DC_BPT_20 };
    let mut s = Session::new(config, TestEv::new(), Capture::default(), Buffers { rx, tx, arena });
    s.handle(Event::Start { secured: false });
    deliver(&mut s, &sap_res(SapCode::OkSuccessfulNegotiation, Some(1)));
    deliver(&mut s, &setup_res(ResponseCode::OkNewSessionEstablished, SID));
    deliver(&mut s, &auth_setup_res(ResponseCode::Ok, &[Authorization::Eim]));
    deliver(&mut s, &auth_res(ResponseCode::Ok, Processing::Finished));
    // The charger offers both energy services; the vehicle asks about DC_BPT.
    let step = deliver(&mut s, &discovery_res(ResponseCode::Ok, &[2, 6]));
    sent_common(&s, &step, |m| {
        let common::Message::ServiceDetailReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.service_id, 6);
    });
    // A set with another BPT channel does not fit; one that matches does.
    const BPT: &[(&str, i32)] = &[("Connector", 1), ("ControlMode", 2), ("MobilityNeedsMode", 1), ("Pricing", 0), ("BPTChannel", 1), ("GeneratorMode", 1)];
    const SEPARATED: &[(&str, i32)] = &[("Connector", 1), ("ControlMode", 2), ("MobilityNeedsMode", 1), ("Pricing", 0), ("BPTChannel", 2), ("GeneratorMode", 1)];
    let step = deliver(&mut s, &detail_res(ResponseCode::Ok, 6, &[(1, SEPARATED), (2, BPT)]));
    sent_common(&s, &step, |m| {
        let common::Message::ServiceSelectionReq(r) = m else { panic!("{m:?}") };
        assert_eq!((r.selected_energy_transfer_service.service_id, r.selected_energy_transfer_service.parameter_set_id), (6, 2));
    });
    let step = deliver(&mut s, &selection_res(ResponseCode::Ok));
    sent_dc(&s, &step, |m| {
        let dc::Message::DcChargeParameterDiscoveryReq(r) = m else { panic!("{m:?}") };
        let dc::DcCpdReqEnergyTransferModeChoice::Bpt(mode) = r.dc_cpd_req_energy_transfer_mode else { panic!("{r:?}") };
        assert_eq!(mode.ev_maximum_discharge_power, rn(3, 11));
    });
    // A base answer is a violation of the selected service in the charge loop; here the BPT answer.
    let bpt_cpd = framed(PayloadType::DC_MAINSTREAM, |out| {
        dc::DcChargeParameterDiscoveryRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Bpt(dc::BptDcCpdResEnergyTransferMode {
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 1000),
                evse_maximum_charge_current: rn(0, 300),
                evse_minimum_charge_current: rn(0, 2),
                evse_maximum_voltage: rn(0, 920),
                evse_minimum_voltage: rn(0, 150),
                evse_power_ramp_limitation: None,
                evse_maximum_discharge_power: rn(3, 12),
                evse_minimum_discharge_power: rn(0, 200),
                evse_maximum_discharge_current: rn(0, 40),
                evse_minimum_discharge_current: rn(0, 2),
            }),
        }
        .encode(out)
        .unwrap()
    });
    deliver(&mut s, &bpt_cpd);
    assert_eq!(s.state(), State::AwaitScheduleExchangeRes);
    deliver(&mut s, &se_res(ResponseCode::Ok, Processing::Finished));
    deliver(&mut s, &cable_res(ResponseCode::Ok, Processing::Finished));
    INLET.with(|i| i.set(BATTERY_V));
    deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V));
    deliver(&mut s, &precharge_res(ResponseCode::Ok, BATTERY_V));
    let step = s.handle(Event::ContactorsClosed);
    sent_common(&s, &step, |m| {
        let common::Message::PowerDeliveryReq(r) = m else { panic!("{m:?}") };
        assert_eq!(r.bpt_channel_selection, Some(common::ChannelSelection::Charge));
    });
    let step = deliver(&mut s, &power_delivery_res(ResponseCode::Ok));
    sent_dc(&s, &step, |m| {
        let dc::Message::DcChargeLoopReq(r) = m else { panic!("{m:?}") };
        let dc::ClReqControlModeChoice::BptDynamicDc(mode) = r.cl_req_control_mode else { panic!("{r:?}") };
        assert_eq!(mode.ev_maximum_discharge_current, rn(0, 30));
    });
    // A plain Dynamic answer in a BPT session ends it.
    let step = deliver(&mut s, &loop_res(None));
    assert!(matches!(step.end, Some(EndReason::UnexpectedMessage)), "{step:?}");
}

#[test]
fn the_vehicle_asks_for_the_pilot_around_the_cable_check_and_the_welding_detection() {
    PILOT.with(|p| p.set(true));
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(None));
    s.handle(Event::StopRequested);
    finish_shutdown(&mut s);
    PILOT.with(|p| p.set(false));
}

#[test]
fn an_emergency_on_the_pilot_ends_the_charge_at_once_without_a_word() {
    use control_pilot::{Pwm, Reading, State as Cp};
    for reading in [
        Reading { state: Cp::C, pwm: Pwm::Off },
        Reading { state: Cp::C, pwm: Pwm::Duty(50) },
        Reading { state: Cp::B, pwm: Pwm::Duty(5) },
    ] {
        let mut rig = Rig::new();
        let mut s = up_to_charge_loop(&mut rig, Capture::default());
        let step = s.handle(Event::Pilot(reading));
        assert_eq!(step.end, Some(EndReason::Emergency(reading)));
        assert_eq!(step.send, None, "nothing is sent");
        assert_eq!(s.state(), State::Ended);
        // Whatever comes later is ignored.
        assert_eq!(deliver(&mut s, &loop_res(None)), Step::default());
    }
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    let reading = Reading { state: Cp::E, pwm: Pwm::Off };
    assert_eq!(s.handle(Event::Pilot(reading)).end, Some(EndReason::PilotFault(reading)));
}

#[test]
fn during_the_stop_the_pilot_may_change_and_the_oscillator_go_off() {
    use control_pilot::{Pwm, Reading, State as Cp};
    let mut rig = Rig::new();
    let mut s = up_to_charge_loop(&mut rig, Capture::default());
    deliver(&mut s, &loop_res(None));
    s.handle(Event::StopRequested);
    // The charger's shutdown ends with the oscillator off, and the vehicle goes to B: no emergency.
    assert_eq!(s.handle(Event::Pilot(Reading { state: Cp::C, pwm: Pwm::Off })).end, None);
    assert_eq!(s.handle(Event::Pilot(Reading { state: Cp::B, pwm: Pwm::Off })).end, None);
}

#[test]
fn the_duty_cycle_before_the_charge_parameters_is_not_judged() {
    use control_pilot::{Pwm, Reading, State as Cp};
    let mut rig = Rig::new();
    let mut s = up_to_charge_parameters(&mut rig, Capture::default());
    assert_eq!(s.handle(Event::Pilot(Reading { state: Cp::B, pwm: Pwm::Off })).end, None);
}
