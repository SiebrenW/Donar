//! The V2G session machine of the EV for ISO 15118-2: SupportedAppProtocol handshake,
//! SessionSetup, ServiceDiscovery, PaymentServiceSelection, Authorization, ChargeParameterDiscovery,
//! then for DC CableCheck, PreCharge, PowerDelivery and the CurrentDemand loop, or for AC
//! PowerDelivery and the ChargingStatus loop, and the shutdown (WeldingDetection for DC) to
//! SessionStop.
//!
//! It follows the same host contract as the ISO 15118-20 [`crate::session::Session`]
//! ([`crate::machine::Machine`]), with two additions the older standard needs: the vehicle changes
//! the control pilot (C before the cable check, B after the energy transfer), which the machine
//! asks for with [`Notice::ControlPilot`] and waits until [`Event::Pilot`] reports the state measured.
//!
//! The code is split by phase: `setup` (up to the charge parameters) and `charge` (the cable
//! check to the end of the session); this file has the public `Session`, the shared state, the
//! dispatch of received frames and the helpers.

use app_protocol;
use iso15118_2::body::{Body, BodyElementChoice};
use iso15118_2::def::V2gMessage;
use iso15118_2::header::MessageHeader;
use iso15118_2::types::*;
use exi::{Arena, DecodeError};
use iso15118_20::common_types::RationalNumber;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::ev::{Caps, DcLimits, Iso2Vehicle};
use crate::log::{LogSink, MsgKind, Record};
use crate::session::{Buffers, EndReason, Event, State, Step};
use crate::{Timer, TimerCmd, Timers};

mod charge;
mod config;
mod convert;
mod setup;
mod tariff;

pub use config::{Config, Mode, MAX_VALUE_ADDED};

/// The most `PMaxScheduleEntry`s kept of the schedule the charger offered (and profile entries sent).
const MAX_ENTRIES: usize = 24;

/// What the charger announced in its charge parameter discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvseLimits {
    pub(crate) max_current: RationalNumber,
    pub(crate) max_power: Option<RationalNumber>,
    pub(crate) max_voltage: RationalNumber,
}

/// The default schedule (V2G-DC-319) the charger offered, for the charging profile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Schedule {
    pub(crate) id: u8,
    /// `(start in s, maximum power in W)`.
    pub(crate) entries: [(u32, f64); MAX_ENTRIES],
    pub(crate) len: usize,
}

pub struct Session<'a, E: Iso2Vehicle, L: LogSink> {
    rx: &'a mut [u8],
    rx_len: usize,
    tx: &'a mut [u8],
    arena: Arena<'a>,
    m: Machine<E, L>,
}

struct Machine<E, L> {
    config: Config,
    ev: Caps<E>,
    log: L,
    state: State,
    /// What the control pilot means to the session (the host reports it with `Event::Pilot`).
    pilot: control_pilot::EvWatch,
    /// The guard the host was last told.
    last_guard: control_pilot::Guard,
    /// The vehicle ends the session for a non-critical reason of its own: it says so with `SessionStopReq` and ends with
    /// this reason when the answer is in (or does not come).
    pending_end: Option<EndReason>,
    /// The id of the session: one zero octet until the charger has assigned one (V2G-DC-241).
    session_id: [u8; 8],
    session_len: usize,
    /// A poll pause is running; when it ends the request of the current state is repeated.
    poll_pending: bool,
    /// The timer of the phase under way (`Ongoing`) is running.
    phase_timed: bool,
    /// The vehicle wants to stop (user, full battery): acted on at the next request.
    stop_requested: bool,
    /// The charger asked to stop in the status of a response (`EVSE_Shutdown`, a notification).
    evse_stops: bool,
    /// `PowerDeliveryReq(ReadyToCharge = TRUE)` has gone out: the charge loop is the way on.
    power_on: bool,
    /// The first `PowerDeliveryRes` arrived (the ready-to-charge timer is stopped).
    ready: bool,
    /// The charge service the vehicle selected.
    service_id: u16,
    /// The challenge of a contract authorization.
    challenge: Option<[u8; 16]>,
    /// The vehicle pays with its contract (Plug & Charge).
    contract: bool,
    /// An AC session (the `ChargingStatus` loop) rather than a DC one.
    ac: bool,
    /// What the vehicle announced in the charge parameter discovery.
    ev_announced: Option<DcLimits>,
    evse: Option<EvseLimits>,
    schedule: Option<Schedule>,
    /// The voltage the vehicle measured at its inlet in the last pre-charge exchange.
    last_inlet: RationalNumber,
    /// A renegotiation of the schedule is under way: stop, `PowerDeliveryReq(Renegotiate)`, the
    /// charge parameter discovery again, then `PowerDeliveryReq(Start)`.
    renegotiating: bool,
    /// The vehicle (host) or the charger (a notification) wants a renegotiation: acted on at the
    /// next loop answer.
    renegotiate_requested: bool,
    /// The session is to be paused: the stop ends with `SessionStopReq(Pause)`.
    pausing: bool,
    /// Value-added services the vehicle asks about (ids), how many are left, and the ones it
    /// selects (with their parameter set).
    detail_queue: [u16; MAX_VALUE_ADDED],
    detail_next: usize,
    detail_len: usize,
    chosen: [SelectedService; MAX_VALUE_ADDED],
    chosen_len: usize,
}

impl<'a, E: Iso2Vehicle, L: LogSink> Session<'a, E, L> {
    pub fn new(config: Config, ev: E, log: L, buffers: Buffers<'a>) -> Self {
        Self {
            rx: buffers.rx,
            rx_len: 0,
            tx: buffers.tx,
            arena: Arena::new(buffers.arena),
            m: Machine {
                config,
                ev: Caps(ev),
                log,
                state: State::Idle,
                pilot: control_pilot::EvWatch::new(matches!(config.mode, Mode::AcSinglePhase | Mode::AcThreePhase)),
                last_guard: control_pilot::Guard::disarmed(control_pilot::Side::Vehicle),
                pending_end: None,
                session_id: [0; 8],
                session_len: 1,
                poll_pending: false,
                phase_timed: false,
                stop_requested: false,
                evse_stops: false,
                power_on: false,
                ready: false,
                service_id: 1,
                challenge: None,
                contract: false,
                ac: matches!(config.mode, Mode::AcSinglePhase | Mode::AcThreePhase),
                ev_announced: None,
                evse: None,
                schedule: None,
                last_inlet: RationalNumber { exponent: 0, value: 0 },
                renegotiating: false,
                renegotiate_requested: false,
                pausing: false,
                detail_queue: [0; MAX_VALUE_ADDED],
                detail_next: 0,
                detail_len: 0,
                chosen: [SelectedService { service_id: 0, parameter_set_id: None }; MAX_VALUE_ADDED],
                chosen_len: 0,
            },
        }
    }

    /// Free space to read the next bytes from the stream into.
    pub fn rx_space(&mut self) -> &mut [u8] {
        &mut self.rx[self.rx_len..]
    }

    /// The outgoing message buffer; valid up to `Step::send` after a step.
    pub fn tx(&self) -> &[u8] {
        self.tx
    }

    pub fn state(&self) -> State {
        self.m.state
    }

    /// Handles an event; see the host contract. The step carries `pilot` when the control pilot guard changed.
    pub fn handle(&mut self, event: Event) -> Step {
        let mut step = self.handle_event(event);
        self.m.announce_guard(&mut step);
        step
    }

    /// What the control pilot is expected to do now (the host arms its own monitor with it).
    pub fn pilot_guard(&self) -> control_pilot::Guard {
        self.m.pilot.guard(self.m.guard_active())
    }

    fn handle_event(&mut self, event: Event) -> Step {
        let Self { rx, rx_len, tx, arena, m } = self;
        if m.state == State::Ended {
            return Step::default();
        }
        match event {
            Event::Start { secured } => {
                if m.state != State::Idle {
                    return Step::default();
                }
                m.log.record(&Record::Started { secured });
                let mut step = m.send_supported_app_protocol_req(tx);
                if step.end.is_none() {
                    step.timers = step
                        .timers
                        .with(TimerCmd::Arm { timer: Timer::Setup, ms: m.config.communication_setup_timeout_ms });
                }
                step
            }
            Event::TimerExpired(timer) => m.on_timer(timer, tx),
            Event::ContactorsClosed => {
                if m.state != State::AwaitContactors {
                    return Step::default();
                }
                let step = m.send_power_delivery(true, tx);
                m.sequence_done(step)
            }
            Event::ContactorsOpened => {
                if m.state != State::AwaitContactorsOpened {
                    return Step::default();
                }
                let step = m.change_control_pilot(crate::session::ControlPilot::B, State::AwaitControlPilotB);
                m.sequence_done(step)
            }
            Event::Pilot(reading) => {
                let step = m.on_pilot(reading);
                if m.awaited_pilot_seen() { m.pilot_arrived(tx) } else { step }
            }
            // The status of an ACDP vehicle: the other protocols have no such stream.
            Event::AcdpStatusChanged => Step::default(),
            Event::ConnectionClosed => match m.pending_end {
                Some(reason) => m.end(reason),
                None => Step::default(),
            },
            Event::ControlPilotChanged => m.pilot_arrived(tx),
            Event::StopRequested => m.on_stop_requested(),
            Event::EnergyTransferStopped => {
                if m.state != State::AwaitCurrentStopped {
                    return Step::default();
                }
                let progress = if m.renegotiating { ChargeProgress::Renegotiate } else { ChargeProgress::Stop };
                let mut step = m.send_power_delivery_as(progress, tx);
                if m.ac && progress == ChargeProgress::Stop && step.end.is_none() {
                    // AC: the pilot goes to B no later than 250 ms after the stop request (V2G2-848); DC does it after the answer.
                    step.notice = Some(crate::session::Notice::ControlPilot(crate::session::ControlPilot::B));
                    m.pilot.set_ready(false);
                }
                m.sequence_done(step)
            }
            // A pause is a stop that ends with `SessionStopReq(Pause)`.
            Event::PauseRequested => {
                m.pausing = true;
                m.on_stop_requested()
            }
            Event::RenegotiateSchedule => m.on_renegotiate_requested(),
            // The older standard has no service renegotiation.
            Event::RenegotiateServices => Step::default(),
            Event::Received(n) => {
                *rx_len = (*rx_len + n).min(rx.len());
                let mut step = Step::default();
                // One frame per call: the step of this frame is the step of the call (see `Machine::frame_pending`).
                match parse(&rx[..*rx_len], rx.len() - HEADER_LEN) {
                    Ok(Parse::Incomplete { .. }) => {}
                    Ok(Parse::Frame(frame)) => {
                        arena.reset();
                        m.log.record(&Record::RxPayload { payload_type: frame.payload_type.0, payload: frame.payload });
                        step = m.on_frame(frame.payload_type, frame.payload, arena, tx);
                        let consumed = frame.consumed;
                        rx.copy_within(consumed..*rx_len, 0);
                        *rx_len -= consumed;
                    }
                    Err(_) => return m.end(EndReason::Framing),
                }
                // A full buffer that still holds no complete frame can never make progress.
                if step.end.is_none() && *rx_len == rx.len() && !crate::machine::frame_buffered(rx, *rx_len) {
                    return m.end(EndReason::Framing);
                }
                step
            }
        }
    }
}

impl<E: Iso2Vehicle, L: LogSink> crate::machine::Machine for Session<'_, E, L> {
    fn handle(&mut self, event: Event) -> Step {
        Session::handle(self, event)
    }
    fn rx_space(&mut self) -> &mut [u8] {
        Session::rx_space(self)
    }
    fn tx(&self) -> &[u8] {
        Session::tx(self)
    }
    fn frame_pending(&self) -> bool {
        crate::machine::frame_buffered(self.rx, self.rx_len)
    }
    fn state(&self) -> State {
        Session::state(self)
    }
    fn pilot_guard(&self) -> control_pilot::Guard {
        Session::pilot_guard(self)
    }
}

/// The `OK*` response codes: a response that carries one is a positive one (9.7.2).
pub fn is_ok(code: ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::Ok
            | ResponseCode::OkNewSessionEstablished
            | ResponseCode::OkOldSessionJoined
            | ResponseCode::OkCertificateExpiresSoon
    )
}

impl<E: Iso2Vehicle, L: LogSink> Machine<E, L> {
    fn is_awaiting(&self, state: State) -> bool {
        matches!(
            state,
            State::AwaitSupportedAppProtocolRes
                | State::AwaitSessionSetupRes
                | State::AwaitServiceDiscoveryRes
                | State::AwaitServiceDetailRes
                | State::AwaitServicePaymentSelectionRes
                | State::AwaitPaymentDetailsRes
                | State::AwaitContractAuthenticationRes
                | State::AwaitChargeParameterDiscoveryRes
                | State::AwaitCableCheckRes
                | State::AwaitPreChargeRes
                | State::AwaitPowerDeliveryRes
                | State::AwaitCurrentDemandRes
                | State::AwaitChargingStatusRes
                | State::AwaitMeteringReceiptRes
                | State::AwaitPowerDeliveryStopRes
                | State::AwaitWeldingDetectionRes
                | State::AwaitSessionStopRes
        )
    }

    fn goto(&mut self, to: State) {
        let from = core::mem::replace(&mut self.state, to);
        self.log.record(&Record::StateChange { from, to });
    }

    fn end(&mut self, reason: EndReason) -> Step {
        // The end that was announced with a `SessionStopReq` is the reason, whatever came instead of the answer; the
        // pilot overrides it.
        let reason = match self.pending_end.take() {
            Some(pending) if !matches!(reason, EndReason::Emergency(_) | EndReason::PilotFault(_)) => pending,
            _ => reason,
        };
        self.goto(State::Ended);
        self.log.record(&Record::Ended { reason });
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Cancel(Timer::Ongoing)),
            end: Some(reason),
            ..Step::default()
        }
    }

    fn on_timer(&mut self, timer: Timer, tx: &mut [u8]) -> Step {
        let state = self.state;
        match timer {
            Timer::Response if self.is_awaiting(state) => self.end(EndReason::Timeout),
            Timer::Ongoing
                if matches!(
                    state,
                    State::AwaitContractAuthenticationRes
                        | State::AwaitChargeParameterDiscoveryRes
                        | State::AwaitCableCheckRes
                        | State::AwaitPreChargeRes
                ) =>
            {
                self.end(EndReason::Timeout)
            }
            // The voltage never fell: the contactors are welded. The pilot is at B by now, so the vehicle says so (and ends).
            Timer::Ongoing if state == State::AwaitWeldingDetectionRes => self.end_politely(EndReason::ContactorWelded, tx),
            Timer::Setup if matches!(state, State::AwaitSupportedAppProtocolRes | State::AwaitSessionSetupRes) => {
                self.end(EndReason::Timeout)
            }
            Timer::ReadyToCharge if !self.ready => self.end(EndReason::Timeout),
            Timer::Sequence
                if matches!(
                    state,
                    State::AwaitControlPilotC
                        | State::AwaitContactors
                        | State::AwaitCurrentStopped
                        | State::AwaitContactorsOpened
                        | State::AwaitControlPilotB
                        | State::AwaitDutyCycle
                ) =>
            {
                self.end(EndReason::Timeout)
            }
            Timer::Poll if self.poll_pending => {
                self.poll_pending = false;
                self.repeat(tx)
            }
            // A timer that was cancelled in the same moment it fired.
            _ => Step::default(),
        }
    }

    /// The header of a request: the session id and nothing else (V2G-DC-548, V2G-DC-619).
    fn header(&self) -> MessageHeader<'_> {
        MessageHeader { session_id: &self.session_id[..self.session_len], notification: None, signature: None }
    }

    /// Frames `body` as a request and sends it, moving to `next`.
    fn send(&mut self, body: BodyElementChoice<'_>, msg: MsgKind, next: State, tx: &mut [u8]) -> Step {
        let message = V2gMessage { header: self.header(), body: Body { body_element: Some(body) } };
        let framed = frame(tx, PayloadType::SAP, |out| message.encode(out));
        self.sent(framed, msg, next)
    }

    /// A request is framed in `tx`: record it, arm the response timer, move to `next`.
    fn sent(&mut self, framed: Result<usize, ()>, msg: MsgKind, next: State) -> Step {
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        let ms = if msg == MsgKind::CurrentDemandReq {
            self.config.current_demand_timeout_ms
        } else if msg == MsgKind::PaymentDetailsReq {
            self.config.payment_details_timeout_ms
        } else {
            self.config.response_timeout_ms
        };
        let step = Step { send: Some(len), timers: Timers::one(TimerCmd::Arm { timer: Timer::Response, ms }), ..Step::default() };
        self.goto(next);
        step
    }

    /// Logs a received response; returns the ending step if its code is a failure (V2G-DC-651,
    /// V2G-DC-653, V2G-DC-654: the session ends in an error shutdown).
    fn received(&mut self, msg: MsgKind, code: ResponseCode) -> Option<Step> {
        self.log.record(&Record::MsgRx { msg });
        self.log.record(&Record::Iso2Response { msg, code });
        if is_ok(code) { None } else { Some(self.end(EndReason::RejectedIso2(code))) }
    }

    /// The vehicle answered (contactors, control pilot, ramp-down): the sequence timer stops.
    fn sequence_done(&mut self, mut step: Step) -> Step {
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Sequence));
        }
        step
    }

    /// A frame that does not decode is ignored; only running out of arena is our own failure.
    fn malformed(&mut self, error: DecodeError) -> Step {
        self.log.record(&Record::DecodeFailed { state: self.state, error });
        if error == DecodeError::ArenaFull { self.end(EndReason::Malformed(error)) } else { Step::default() }
    }

    /// Starts the timer of a phase (cable check, pre-charge, `Ongoing` exchanges) once, with the
    /// first request of the phase.
    fn time_phase(&mut self, mut step: Step, ms: u32) -> Step {
        if step.end.is_none() && !self.phase_timed {
            self.phase_timed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms });
        }
        step
    }

    /// The phase is over: its timer stops.
    fn phase_done(&mut self, mut step: Step) -> Step {
        if self.phase_timed && step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Ongoing));
        }
        self.phase_timed = false;
        step
    }

    /// Pause for the poll interval (if any), then repeat the request of the current state.
    fn pause_then_repeat(&mut self, tx: &mut [u8]) -> Step {
        if self.config.poll_interval_ms == 0 {
            return self.repeat(tx);
        }
        self.poll_pending = true;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                .with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.poll_interval_ms }),
            ..Step::default()
        }
    }

    /// Sends the request of the current state again.
    fn repeat(&mut self, tx: &mut [u8]) -> Step {
        match self.state {
            State::AwaitContractAuthenticationRes => self.send_authorization_req(tx),
            State::AwaitChargeParameterDiscoveryRes => self.send_charge_parameter_discovery_req(tx),
            State::AwaitServiceDetailRes => self.send_service_detail_req(tx),
            State::AwaitCableCheckRes => self.send_cable_check_req(tx),
            State::AwaitPreChargeRes => self.send_precharge_req(tx),
            State::AwaitCurrentDemandRes => self.send_current_demand_req(tx),
            State::AwaitChargingStatusRes => self.send_charging_status_req(tx),
            State::AwaitWeldingDetectionRes => self.send_welding_req(tx),
            _ => Step::default(),
        }
    }

    /// Sends what comes next, unless the vehicle wants to stop and has not yet asked for power
    /// (V2G-DC-648): then `SessionStopReq` is the next request.
    fn next(&mut self, tx: &mut [u8], f: impl FnOnce(&mut Self, &mut [u8]) -> Step) -> Step {
        if self.stop_requested && !self.power_on {
            self.stop_requested = false;
            return self.send_session_stop(tx);
        }
        f(self, tx)
    }

    /// Whether a session is under way, for the control pilot guard.
    fn guard_active(&self) -> bool {
        !matches!(self.state, State::Idle | State::Ended)
    }

    /// Tells the host when what is expected of the control pilot changed.
    fn announce_guard(&mut self, step: &mut Step) {
        let guard = self.pilot.guard(self.guard_active());
        if guard != self.last_guard {
            self.last_guard = guard;
            step.pilot = Some(guard);
            self.log.record(&Record::PilotGuard { guard });
        }
    }

    /// Whether the pilot state the vehicle asked for (C or D before the cable check, B before the welding detection) has
    /// been measured: setting the resistor is not seeing the change.
    fn awaited_pilot_seen(&self) -> bool {
        let last = self.pilot.last();
        match self.state {
            State::AwaitControlPilotC => last.is_some_and(control_pilot::Reading::is_ready),
            State::AwaitControlPilotB => last.is_some_and(|r| r.state == control_pilot::State::B),
            State::AwaitDutyCycle => last.is_some_and(|r| r.pwm.is_five_percent() || r.pwm.is_nominal()),
            _ => false,
        }
    }

    /// The next request after a pilot change, once the change has been measured.
    fn pilot_arrived(&mut self, tx: &mut [u8]) -> Step {
        if !self.awaited_pilot_seen() {
            return Step::default();
        }
        match self.state {
            State::AwaitControlPilotC => {
                self.pilot.set_ready(true);
                let step = self.send_cable_check_req(tx);
                self.sequence_done(step)
            }
            State::AwaitDutyCycle => {
                let step = self.send_charge_parameter_discovery_req(tx);
                self.sequence_done(step)
            }
            _ => {
                let step = self.after_stop(tx);
                self.sequence_done(step)
            }
        }
    }

    /// A change of the control pilot (IEC 61851-23 CC.3.4.3, CC.4.3; V2G-DC-647, 655, 656): an emergency or a
    /// lost connection ends the session at once and sends nothing.
    fn on_pilot(&mut self, reading: control_pilot::Reading) -> Step {
        use control_pilot::Verdict;
        let verdict = self.pilot.reading(reading);
        if self.state == State::Idle {
            return Step::default();
        }
        match verdict {
            Verdict::Fine => Step::default(),
            Verdict::SupplyStopped => self.on_stop_requested(),
            Verdict::Fault => self.end(EndReason::PilotFault(reading)),
            Verdict::Emergency => self.end(EndReason::Emergency(reading)),
        }
    }

    fn on_stop_requested(&mut self) -> Step {
        match self.state {
            // Between two charge loop requests: stop at once.
            State::AwaitCurrentDemandRes | State::AwaitChargingStatusRes if self.poll_pending => {
                self.poll_pending = false;
                self.begin_stop()
            }
            State::Idle | State::Ended => Step::default(),
            // Later: at the next request (V2G-DC-648) or the next charge loop response (V2G-DC-673).
            _ => {
                self.stop_requested = true;
                Step::default()
            }
        }
    }

    /// A frame arrived: decode it according to what the machine waits for and run the handler.
    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
        if payload_type != PayloadType::SAP {
            return self.unexpected(payload_type);
        }
        if self.state == State::AwaitSupportedAppProtocolRes {
            return match app_protocol::decode(payload, arena) {
                Ok(app_protocol::Message::SupportedAppProtocolRes(res)) => {
                    self.log.record(&Record::MsgRx { msg: MsgKind::SupportedAppProtocolRes });
                    self.on_supported_app_protocol_res(res, tx)
                }
                Ok(_) => self.unexpected(payload_type),
                Err(e) => self.malformed(e),
            };
        }
        if !self.is_awaiting(self.state) {
            return self.unexpected(payload_type);
        }
        let message = match V2gMessage::decode(payload, arena) {
            Ok(m) => m,
            Err(e) => return self.malformed(e),
        };
        let Some(body) = message.body.body_element else { return self.unexpected(payload_type) };
        use BodyElementChoice as B;
        match (self.state, body) {
            (State::AwaitSessionSetupRes, B::SessionSetupRes(res)) => self.on_session_setup_res(message.header.session_id, res, tx),
            (State::AwaitServiceDiscoveryRes, B::ServiceDiscoveryRes(res)) => self.on_service_discovery_res(res, tx),
            (State::AwaitServiceDetailRes, B::ServiceDetailRes(res)) => self.on_service_detail_res(res, tx),
            (State::AwaitServicePaymentSelectionRes, B::PaymentServiceSelectionRes(res)) => self.on_payment_service_selection_res(res, tx),
            (State::AwaitPaymentDetailsRes, B::PaymentDetailsRes(res)) => self.on_payment_details_res(res, tx),
            (State::AwaitContractAuthenticationRes, B::AuthorizationRes(res)) => self.on_authorization_res(res, tx),
            (State::AwaitChargeParameterDiscoveryRes, B::ChargeParameterDiscoveryRes(res)) => self.on_charge_parameter_discovery_res(res, tx),
            (State::AwaitCableCheckRes, B::CableCheckRes(res)) => self.on_cable_check_res(res, tx),
            (State::AwaitPreChargeRes, B::PreChargeRes(res)) => self.on_precharge_res(res, tx),
            (State::AwaitPowerDeliveryRes, B::PowerDeliveryRes(res)) => self.on_power_delivery_start_res(res, tx),
            (State::AwaitCurrentDemandRes, B::CurrentDemandRes(res)) => self.on_current_demand_res(res, tx),
            (State::AwaitPowerDeliveryStopRes, B::PowerDeliveryRes(res)) => self.on_power_delivery_stop_res(res, tx),
            (State::AwaitChargingStatusRes, B::ChargingStatusRes(res)) => self.on_charging_status_res(res, tx),
            (State::AwaitMeteringReceiptRes, B::MeteringReceiptRes(res)) => self.on_metering_receipt_res(res, tx),
            (State::AwaitWeldingDetectionRes, B::WeldingDetectionRes(res)) => self.on_welding_detection_res(res, tx),
            (State::AwaitSessionStopRes, B::SessionStopRes(res)) => self.on_session_stop_res(res),
            _ => self.unexpected(payload_type),
        }
    }

    /// V2G-DC-402: a response that is not the one the request in flight asks for ends the session.
    fn unexpected(&mut self, payload_type: PayloadType) -> Step {
        self.log.record(&Record::Ignored { state: self.state, payload_type: payload_type.0 });
        self.end(EndReason::UnexpectedMessage)
    }
}

/// Encodes a payload behind a V2GTP header at the start of `tx`; returns the framed length.
fn frame(
    tx: &mut [u8],
    payload_type: PayloadType,
    encode: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>,
) -> Result<usize, ()> {
    let n = encode(tx.get_mut(HEADER_LEN..).ok_or(())?).map_err(|_| ())?;
    write_header(tx, payload_type, n).map_err(|_| ())?;
    Ok(HEADER_LEN + n)
}
