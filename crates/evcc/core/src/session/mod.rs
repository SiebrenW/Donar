//! The V2G session machine of the EV: SupportedAppProtocol handshake, SessionSetup, EIM
//! authorization, service discovery and selection, charge parameters, schedule exchange, cable
//! check, pre-charge, power delivery, the Dynamic DC charge loop and the shutdown to
//! SessionStop.
//!
//! The code is split by phase: `setup`, `parameters`, `charge` and `shutdown` hold the
//! handlers of the responses and the requests that follow them; this file has the public
//! `Session`, the shared state of the machine, the dispatch of received frames and the helpers
//! they use.
//!
//! Host contract:
//! 1. Open the TCP/TLS connection found by SDP, then send [`Event::Start`].
//! 2. Read from the socket straight into [`Session::rx_space`], then send
//!    [`Event::Received`] with the byte count.
//! 3. After every call, carry out the returned [`Step`]: write `tx[..send]` to the stream,
//!    apply the timer commands, handle `notice`, and close the connection once `end` is set.
//! 4. When an armed timer fires, send [`Event::TimerExpired`] naming it.

use app_protocol;
use exi::{Arena, DecodeError};
use iso15118_20::common::{self, SelectedService};
use iso15118_20::common_types::{MessageHeader, ResponseCode};
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::{HEADER_LEN, Parse, PayloadType, parse, write_header};

use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

mod charge;
mod config;
mod limits;
mod metering;
mod parameters;
mod renegotiation;
mod schedule;
mod setup;
mod shutdown;
mod tariff;
mod types;

pub use config::{Config, SERVICE_ID_AC, SERVICE_ID_AC_BPT, SERVICE_ID_DC, SERVICE_ID_DC_BPT};
pub use types::{AcChargeLoopStatus, Buffers, ChargeLoopStatus, MeterReading, ControlPilot, EndReason, Event, Notice, Resume, State, Step};

pub struct Session<'a, E: Ev, L: LogSink> {
    rx: &'a mut [u8],
    rx_len: usize,
    tx: &'a mut [u8],
    arena: Arena<'a>,
    m: Machine<E, L>,
}

struct Machine<E, L> {
    config: Config,
    ev: E,
    log: L,
    state: State,
    /// What the control pilot means to the session (the host reports it with `Event::Pilot`).
    pilot: control_pilot::EvWatch,
    /// The vehicle ends the session for a non-critical reason of its own: it says so with `SessionStopReq` and ends
    /// with this reason when the answer is in (or does not come).
    pending_end: Option<EndReason>,
    termination_code: Option<&'static str>,
    secured: bool,
    session_id: [u8; 8],
    /// The `Ongoing` timer is armed once, when the first `Ongoing` response arrives.
    ongoing_armed: bool,
    /// A poll pause is running; when it ends the request of the current state is repeated.
    poll_pending: bool,
    /// The last `DC_PreChargeReq` carried `EVProcessing = Finished`.
    precharge_finishing: bool,
    /// The last `DC_WeldingDetectionReq` carried `EVProcessing = Finished`.
    welding_finishing: bool,
    /// The vehicle asked to stop while a charge loop request was outstanding.
    stop_requested: bool,
    /// `ServiceRenegotiationSupported` of the first `ServiceDiscoveryRes` (V2G20-1480).
    renegotiation_supported: Option<bool>,
    /// The shutdown under way ends in `SessionStopReq(ServiceRenegotiation)` and a new
    /// service discovery instead of the end of the session.
    stop_kind: types::StopKind,
    /// The schedule renegotiation side stream.
    side: renegotiation::Side,
    side_waited_ms: u32,
    /// The side timer is a pause before repeating `ScheduleExchangeReq`, not a response timeout.
    side_poll_pending: bool,
    /// At least one `ChargeLoopReq` went out in this charge (V2G20-1846).
    charge_loop_started: bool,
    /// The host is ramping the current down to enter a standby period.
    entering_standby: bool,
    /// A standby period is on (the EVSE accepted `PowerDeliveryReq(Standby)`).
    standby: bool,
    /// SECC time minus the vehicle's clock, learned from `SessionSetupRes` (V2G20-1536).
    clock_offset: i128,
    last_timestamp: u64,
    selected: SelectedService,
    /// The authorization service in use, and what the charger offered (for the fallback to EIM).
    authorization: common::Authorization,
    eim_offered: bool,
    /// The challenge of `AuthorizationSetupRes` a Plug & Charge request answers.
    pnc_challenge: Option<[u8; 16]>,
    /// The limits the vehicle announced in its last charge parameter discovery and the charger's
    /// answer to it, which the charge loop has to stay within (V2G20-2198, 2199, 2654, 2655).
    ev_announced: Option<crate::ev::DcLimits>,
    ev_announced_discharge: Option<crate::ev::DischargeLimits>,
    evse_announced: Option<dc::DcCpdResEnergyTransferMode>,
    /// Scheduled control mode: the schedule tuple the vehicle selected, and the power of its
    /// first entry (what the vehicle draws when the charger names no target).
    selected_tuple: u32,
    schedule: schedule::Schedule,
    /// The charger said `GoToPause`: the power delivery stop that is under way ends in a pause, with the power never on.
    paused_at_start: bool,
    /// `MobilityNeedsMode` 2: what the charger named of the departure time, the minimum and the target
    /// state of charge, and the vehicle's clock (microseconds) when it did.
    mobility: crate::ev::MobilityNeeds,
    mobility_at: u64,
    /// The connector of the selected AC parameter set is ThreePhase (V2G20-1815 to 1818: the
    /// phase specific elements are used, or not, accordingly).
    ac_three_phase: bool,
    tolerance_offered: bool,
    /// The same for an AC session.
    ev_announced_ac: Option<crate::ev::AcPowerLimits>,
    ev_announced_ac_discharge: Option<crate::ev::AcDischargePowerLimits>,
    evse_announced_ac: Option<ac::AcCpdResEnergyTransferMode>,
    /// The voltage at the inlet as the vehicle last measured it (side B during pre-charge).
    last_inlet: iso15118_20::common_types::RationalNumber,
}

impl<'a, E: Ev, L: LogSink> Session<'a, E, L> {
    pub fn new(config: Config, ev: E, log: L, buffers: Buffers<'a>) -> Self {
        let ac = config.ac();
        Self {
            rx: buffers.rx,
            rx_len: 0,
            tx: buffers.tx,
            arena: Arena::new(buffers.arena),
            m: Machine {
                config,
                ev,
                log,
                state: State::Idle,
                pilot: control_pilot::EvWatch::new(ac),
                pending_end: None,
                termination_code: None,
                secured: false,
                session_id: [0; 8],
                ongoing_armed: false,
                poll_pending: false,
                precharge_finishing: false,
                welding_finishing: false,
                stop_requested: false,
                renegotiation_supported: None,
                side: renegotiation::Side::Idle,
                side_waited_ms: 0,
                side_poll_pending: false,
                charge_loop_started: false,
                stop_kind: types::StopKind::Terminate,
                entering_standby: false,
                standby: false,
                clock_offset: 0,
                last_timestamp: 0,
                selected: SelectedService { service_id: 0, parameter_set_id: 0 },
                authorization: common::Authorization::Eim,
                eim_offered: false,
                pnc_challenge: None,
                ev_announced: None,
                ev_announced_discharge: None,
                evse_announced: None,
                selected_tuple: 0,
                paused_at_start: false,
                schedule: schedule::Schedule::NONE,
                mobility: crate::ev::MobilityNeeds::default(),
                mobility_at: 0,
                ac_three_phase: false,
                tolerance_offered: false,
                ev_announced_ac: None,
                ev_announced_ac_discharge: None,
                evse_announced_ac: None,
                last_inlet: iso15118_20::common_types::RationalNumber { exponent: 0, value: 0 },
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

    /// The mobility needs the charger named (`MobilityNeedsMode` 2): the latest value of each field.
    /// The departure time is as it was received.
    pub fn mobility_needs(&self) -> crate::ev::MobilityNeeds {
        self.m.mobility
    }

    pub fn state(&self) -> State {
        self.m.state
    }

    pub fn handle(&mut self, event: Event) -> Step {
        let Self { rx, rx_len, tx, arena, m } = self;
        if m.state == State::Ended {
            return Step::default();
        }
        match event {
            Event::Start { secured } => {
                if m.state != State::Idle {
                    return Step::default();
                }
                m.secured = secured;
                m.log.record(&Record::Started { secured });
                if !secured && !m.config.allow_unsecured {
                    return m.end(EndReason::TlsRequired);
                }
                let mut step = m.send_supported_app_protocol_req(tx);
                if step.end.is_none() {
                    step.timers = step
                        .timers
                        .with(TimerCmd::Arm { timer: Timer::Setup, ms: m.config.communication_setup_timeout_ms });
                }
                step
            }
            Event::TimerExpired(timer) => match (timer, m.state) {
                (Timer::Response, s) if m.is_awaiting(s) => m.end(EndReason::Timeout),
                (
                    Timer::Ongoing,
                    State::AwaitAuthorizationRes
                    | State::AwaitScheduleExchangeRes
                    | State::AwaitCableCheckRes
                    | State::AwaitPreChargeRes,
                ) => m.end(EndReason::Timeout),
                (Timer::Ongoing, State::AwaitWeldingDetectionRes) => m.end(EndReason::ContactorWelded),
                (Timer::Setup, State::AwaitSupportedAppProtocolRes | State::AwaitSessionSetupRes) => {
                    m.end(EndReason::Timeout)
                }
                (
                    Timer::Sequence,
                    State::AwaitContactors
                    | State::AwaitCurrentStopped
                    | State::AwaitContactorsOpened
                    | State::AwaitControlPilotC
                    | State::AwaitControlPilotB,
                ) => m.end(EndReason::Timeout),
                (Timer::Side, _) => m.on_side_timer(tx),
                (Timer::Poll, _) if m.poll_pending => {
                    m.poll_pending = false;
                    m.repeat(tx)
                }
                // A timer that was cancelled in the same moment it fired.
                _ => Step::default(),
            },
            Event::ContactorsClosed => {
                if m.state != State::AwaitContactors {
                    return Step::default();
                }
                m.pilot.set_ready(true);
                let step = m.send_power_delivery(common::ChargeProgress::Start, tx);
                m.sequence_done(step)
            }
            Event::ContactorsOpened => {
                if m.state != State::AwaitContactorsOpened {
                    return Step::default();
                }
                // V2G20-919: the pilot goes to B when the contactors are open, then welding detection.
                if m.config.control_pilot {
                    return m.change_control_pilot(types::ControlPilot::B, State::AwaitControlPilotB);
                }
                let step = m.send_welding_req(false, tx);
                m.sequence_done(step)
            }
            Event::StopRequested => m.stop_requested_by_vehicle(),
            Event::PauseRequested => match m.state {
                State::AwaitChargeLoopRes if m.poll_pending => {
                    m.poll_pending = false;
                    m.stop_kind = types::StopKind::Pause;
                    m.begin_stop(None)
                }
                State::AwaitChargeLoopRes => {
                    m.stop_kind = types::StopKind::Pause;
                    m.stop_requested = true;
                    Step::default()
                }
                _ => Step::default(),
            },
            Event::RenegotiateSchedule => m.start_schedule_renegotiation(Step::default(), tx),
            Event::RenegotiateServices => {
                if m.renegotiation_supported != Some(true) {
                    return Step::default();
                }
                match m.state {
                    State::AwaitChargeLoopRes if m.poll_pending => {
                        m.poll_pending = false;
                        m.stop_kind = types::StopKind::ServiceRenegotiation;
                        m.begin_stop(None)
                    }
                    State::AwaitChargeLoopRes => {
                        m.stop_kind = types::StopKind::ServiceRenegotiation;
                        m.stop_requested = true;
                        Step::default()
                    }
                    _ => Step::default(),
                }
            }
            Event::Pilot(reading) => m.on_pilot(reading),
            Event::ControlPilotChanged => match m.state {
                State::AwaitControlPilotC => {
                    m.pilot.set_ready(true);
                    let mut step = m.send_cable_check_req(tx);
                    if step.end.is_none() {
                        step.notice = Some(Notice::ScheduleExchanged);
                    }
                    m.sequence_done(step)
                }
                State::AwaitControlPilotB => {
                    let step = m.send_welding_req(false, tx);
                    m.sequence_done(step)
                }
                _ => Step::default(),
            },
            Event::EnergyTransferStopped => {
                if m.state != State::AwaitCurrentStopped {
                    return Step::default();
                }
                let progress = if core::mem::take(&mut m.entering_standby) {
                    common::ChargeProgress::Standby
                } else {
                    common::ChargeProgress::Stop
                };
                let step = m.send_power_delivery(progress, tx);
                m.sequence_done(step)
            }
            Event::Received(n) => {
                *rx_len = (*rx_len + n).min(rx.len());
                let mut step = Step::default();
                loop {
                    let consumed = match parse(&rx[..*rx_len], rx.len() - HEADER_LEN) {
                        Ok(Parse::Incomplete { .. }) => break,
                        Ok(Parse::Frame(frame)) => {
                            arena.reset();
                            m.log.record(&Record::RxPayload { payload_type: frame.payload_type.0, payload: frame.payload });
                            step = m.on_frame(frame.payload_type, frame.payload, arena, tx);
                            frame.consumed
                        }
                        Err(_) => return m.end(EndReason::Framing),
                    };
                    rx.copy_within(consumed..*rx_len, 0);
                    *rx_len -= consumed;
                    if step.end.is_some() {
                        break;
                    }
                }
                // A full buffer that still holds no complete frame can never make progress.
                if step.end.is_none() && *rx_len == rx.len() {
                    return m.end(EndReason::Framing);
                }
                step
            }
        }
    }
}

impl<E: Ev, L: LogSink> crate::machine::Machine for Session<'_, E, L> {
    fn handle(&mut self, event: Event) -> Step {
        Session::handle(self, event)
    }
    fn rx_space(&mut self) -> &mut [u8] {
        Session::rx_space(self)
    }
    fn tx(&self) -> &[u8] {
        Session::tx(self)
    }
    fn state(&self) -> State {
        Session::state(self)
    }
}

/// The `OK*` response codes (ISO 15118-20:2022 Annex A, response code table): which message
/// may carry which is checked by the EVSE side; the EVCC accepts all of them.
pub(crate) fn is_ok(code: ResponseCode) -> bool {
    matches!(
        code,
        ResponseCode::Ok
            | ResponseCode::OkCertificateExpiresSoon
            | ResponseCode::OkNewSessionEstablished
            | ResponseCode::OkOldSessionJoined
            | ResponseCode::OkPowerToleranceConfirmed
    )
}

impl<E: Ev, L: LogSink> Machine<E, L> {
    fn is_awaiting(&self, state: State) -> bool {
        matches!(
            state,
            State::AwaitSupportedAppProtocolRes
                | State::AwaitSessionSetupRes
                | State::AwaitAuthorizationSetupRes
                | State::AwaitAuthorizationRes
                | State::AwaitServiceDiscoveryRes
                | State::AwaitServiceDetailRes
                | State::AwaitServiceSelectionRes
                | State::AwaitChargeParameterDiscoveryRes
                | State::AwaitScheduleExchangeRes
                | State::AwaitCableCheckRes
                | State::AwaitPreChargeRes
                | State::AwaitPowerDeliveryRes
                | State::AwaitPowerDeliveryStandbyRes
                | State::AwaitChargeLoopRes
                | State::AwaitPowerDeliveryStopRes
                | State::AwaitWeldingDetectionRes
                | State::AwaitSessionStopRes
        )
    }
    fn goto(&mut self, to: State) {
        let from = core::mem::replace(&mut self.state, to);
        self.log.record(&Record::StateChange { from, to });
    }
    /// The vehicle wants to stop (user, full battery, fault, the AC supply gone): acted on while the charge loop runs.
    fn stop_requested_by_vehicle(&mut self) -> Step {
        self.stop_kind = types::StopKind::Terminate;
        match self.state {
            State::AwaitChargeLoopRes if self.poll_pending => {
                self.poll_pending = false;
                self.begin_stop(None)
            }
            // Also while a standby period is being entered: acted on once it is on.
            State::AwaitChargeLoopRes | State::AwaitPowerDeliveryStandbyRes => {
                self.stop_requested = true;
                Step::default()
            }
            State::AwaitCurrentStopped if self.entering_standby => {
                self.stop_requested = true;
                Step::default()
            }
            _ => Step::default(),
        }
    }

    /// A change of the control pilot (IEC 61851-23 CC.3.4.3, CC.4.3; V2G-DC-655, 656): an emergency or a lost
    /// connection ends the session at once and sends nothing; the loss of an AC supply is an orderly stop.
    fn on_pilot(&mut self, reading: control_pilot::Reading) -> Step {
        use control_pilot::Verdict;
        let verdict = self.pilot.reading(reading);
        if matches!(self.state, State::Idle | State::Ended) {
            return Step::default();
        }
        match verdict {
            Verdict::Fine => Step::default(),
            Verdict::SupplyStopped => self.stop_requested_by_vehicle(),
            Verdict::Fault => self.end(EndReason::PilotFault(reading)),
            Verdict::Emergency => self.end(EndReason::Emergency(reading)),
        }
    }

    /// The control pilot change the host is asked for (`Config::control_pilot`).
    fn change_control_pilot(&mut self, to: types::ControlPilot, next: State) -> Step {
        if to == types::ControlPilot::B {
            self.pilot.set_ready(false);
        }
        self.goto(next);
        Step {
            notice: Some(Notice::ControlPilot(to)),
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(self.arm_sequence()),
            ..Step::default()
        }
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
    /// The header `TimeStamp`: microseconds in SECC time (V2G20-1534), increasing with every
    /// message (V2G20-1537). Until the EVSE's clock is known (its `SessionSetupRes`) it is the
    /// vehicle's own best guess of UTC, or zero (V2G20-1535).
    fn timestamp(&mut self) -> u64 {
        let local = i128::from(self.ev.now_micros());
        let t = u64::try_from((local + self.clock_offset).max(0)).unwrap_or(u64::MAX);
        let t = t.max(self.last_timestamp.saturating_add(1));
        self.last_timestamp = t;
        t
    }
    /// SECC time, microseconds (the time stamps of the messages are in it, V2G20-1534).
    fn secc_now(&self) -> u64 {
        u64::try_from((i128::from(self.ev.now_micros()) + self.clock_offset).max(0)).unwrap_or(u64::MAX)
    }
    fn header(&mut self) -> MessageHeader<'static> {
        MessageHeader::new(self.session_id, self.timestamp())
    }
    /// Records the transmit and arms the response timer for a message framed by [`frame`].
    fn sent(&mut self, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        Step {
            send: Some(len),
            timers: Timers::one(TimerCmd::Arm {
                timer: Timer::Response,
                ms: if msg == MsgKind::ServiceDetailReq {
                    self.config.service_detail_timeout_ms
                } else if matches!(msg, MsgKind::DcChargeLoopReq | MsgKind::AcChargeLoopReq) {
                    self.config.charge_loop_timeout_ms
                } else {
                    self.config.response_timeout_ms
                },
            }),
            ..Step::default()
        }
    }
    /// Sends `msg` and, unless the send failed, moves to `next`.
    fn send_then(&mut self, next: State, msg: MsgKind, framed: Result<usize, ()>) -> Step {
        let step = self.sent(msg, framed);
        if step.end.is_none() {
            self.goto(next);
        }
        step
    }
    /// V2G20-440: a message that is not a valid response to the request in flight is ignored;
    /// the response timer keeps running.
    fn ignore(&mut self, payload_type: PayloadType) -> Step {
        self.log.record(&Record::Ignored { state: self.state, payload_type: payload_type.0 });
        Step::default()
    }
    /// The EVSE answered `Ongoing`: pause for the poll interval (if any), then repeat the
    /// request of the current state.
    fn ongoing_repeat(&mut self, tx: &mut [u8]) -> Step {
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
    /// Sends the request of the current state again and makes sure the `Ongoing` bound runs.
    fn repeat(&mut self, tx: &mut [u8]) -> Step {
        let step = match self.state {
            State::AwaitAuthorizationRes => self.send_authorization_req(State::AwaitAuthorizationRes, tx),
            State::AwaitScheduleExchangeRes => self.send_schedule_exchange_req(tx),
            State::AwaitCableCheckRes => self.send_cable_check_req(tx),
            State::AwaitPreChargeRes => self.send_precharge_req(false, tx),
            // The charge loop has its own cycle and no `Ongoing` bound.
            State::AwaitChargeLoopRes => return self.send_charge_loop_req(tx),
            State::AwaitWeldingDetectionRes => self.send_welding_req(false, tx),
            _ => return Step::default(),
        };
        self.arm_ongoing(step)
    }
    /// Arms the overall `Ongoing` bound once, when the first `Ongoing` answer arrives.
    fn arm_ongoing(&mut self, mut step: Step) -> Step {
        if step.end.is_none() && !self.ongoing_armed {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.ongoing_timeout_ms });
        }
        step
    }
    /// Logs a received response and its code; returns the ending step if the code is a failure.
    fn received(&mut self, msg: MsgKind, code: ResponseCode) -> Option<Step> {
        self.received_or(msg, code, None)
    }
    /// Like [`Self::received`], but `also_ok` (a `WARNING_*` code the message tolerates) does not
    /// end the session. Every other WARNING is treated as an error: the standard lets the EVCC
    /// stop where it does not define a remedy (V2G20-1583).
    fn received_or(&mut self, msg: MsgKind, code: ResponseCode, also_ok: Option<ResponseCode>) -> Option<Step> {
        self.log.record(&Record::MsgRx { msg });
        self.log.record(&Record::Response { msg, code });
        self.check_response_code(msg, code);
        if is_ok(code) || Some(code) == also_ok { None } else { Some(self.end(EndReason::Rejected(code))) }
    }
    /// Notes a response code the message may not carry.
    fn check_response_code(&mut self, msg: MsgKind, code: ResponseCode) {
        if msg.response_message().is_some_and(|m| !m.allows(code)) {
            self.log.record(&Record::ResponseCodeNotAllowed { msg, code });
        }
    }
    fn arm_sequence(&self) -> TimerCmd {
        TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }
    }
    /// The vehicle answered: the sequence timer stops.
    fn sequence_done(&mut self, mut step: Step) -> Step {
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Sequence));
        }
        step
    }
    /// A frame that does not decode is ignored like any other invalid message (V2G20-440);
    /// only running out of arena is our own failure.
    fn malformed(&mut self, error: DecodeError) -> Step {
        self.log.record(&Record::DecodeFailed { state: self.state, error });
        if error == DecodeError::ArenaFull { self.end(EndReason::Malformed(error)) } else { Step::default() }
    }
    /// Decodes a frame that must be a mainstream message and hands it to `f`; a message of a
    /// kind `f` does not want (`None`) is ignored like any other unexpected one.
    fn mainstream(
        &mut self,
        payload_type: PayloadType,
        payload: &[u8],
        arena: &Arena<'_>,
        f: impl FnOnce(&mut Self, common::Message<'_>) -> Option<Step>,
    ) -> Step {
        if payload_type != PayloadType::MAINSTREAM {
            return self.ignore(payload_type);
        }
        match common::decode(payload, arena) {
            Ok(msg) => f(self, msg).unwrap_or_else(|| self.ignore(payload_type)),
            Err(e) => self.malformed(e),
        }
    }

    /// The same for DC mainstream messages.
    fn dc_mainstream(
        &mut self,
        payload_type: PayloadType,
        payload: &[u8],
        arena: &Arena<'_>,
        f: impl FnOnce(&mut Self, dc::Message<'_>) -> Option<Step>,
    ) -> Step {
        if payload_type != PayloadType::DC_MAINSTREAM {
            return self.ignore(payload_type);
        }
        match dc::decode(payload, arena) {
            Ok(msg) => f(self, msg).unwrap_or_else(|| self.ignore(payload_type)),
            Err(e) => self.malformed(e),
        }
    }

    /// The same for AC mainstream messages.
    fn ac_mainstream(
        &mut self,
        payload_type: PayloadType,
        payload: &[u8],
        arena: &Arena<'_>,
        f: impl FnOnce(&mut Self, ac::Message<'_>) -> Option<Step>,
    ) -> Step {
        if payload_type != PayloadType::AC_MAINSTREAM {
            return self.ignore(payload_type);
        }
        match ac::decode(payload, arena) {
            Ok(msg) => f(self, msg).unwrap_or_else(|| self.ignore(payload_type)),
            Err(e) => self.malformed(e),
        }
    }

    /// A frame arrived: decode it according to what the machine waits for and run the handler.
    fn on_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
        use common::Message as M;
        if payload_type == PayloadType::SCHEDULE_RENEGOTIATION {
            return self.on_side_frame(payload_type, payload, arena, tx);
        }
        if payload_type == PayloadType::METERING_CONFIRMATION {
            return self.on_metering_frame(payload_type, payload, arena);
        }
        match self.state {
            State::AwaitSupportedAppProtocolRes if payload_type == PayloadType::SAP => {
                match app_protocol::decode(payload, arena) {
                    Ok(app_protocol::Message::SupportedAppProtocolRes(res)) => {
                        self.log.record(&Record::MsgRx { msg: MsgKind::SupportedAppProtocolRes });
                        self.on_supported_app_protocol_res(res, tx)
                    }
                    Ok(_) => self.ignore(payload_type),
                    Err(e) => self.malformed(e),
                }
            }
            State::AwaitSessionSetupRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::SessionSetupRes(res) => Some(m.on_session_setup_res(res, tx)),
                _ => None,
            }),
            State::AwaitAuthorizationSetupRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::AuthorizationSetupRes(res) => Some(m.on_authorization_setup_res(res, tx)),
                _ => None,
            }),
            State::AwaitAuthorizationRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::AuthorizationRes(res) => Some(m.on_authorization_res(res, tx)),
                _ => None,
            }),
            State::AwaitServiceDiscoveryRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::ServiceDiscoveryRes(res) => Some(m.on_service_discovery_res(res, tx)),
                _ => None,
            }),
            State::AwaitServiceDetailRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::ServiceDetailRes(res) => Some(m.on_service_detail_res(res, tx)),
                _ => None,
            }),
            State::AwaitServiceSelectionRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::ServiceSelectionRes(res) => Some(m.on_service_selection_res(res, tx)),
                _ => None,
            }),
            State::AwaitChargeParameterDiscoveryRes if self.config.ac() => {
                self.ac_mainstream(payload_type, payload, arena, |m, msg| match msg {
                    ac::Message::AcChargeParameterDiscoveryRes(res) => Some(m.on_ac_charge_parameter_discovery_res(res, tx)),
                    _ => None,
                })
            }
            State::AwaitChargeParameterDiscoveryRes => {
                self.dc_mainstream(payload_type, payload, arena, |m, msg| match msg {
                    dc::Message::DcChargeParameterDiscoveryRes(res) => Some(m.on_charge_parameter_discovery_res(res, tx)),
                    _ => None,
                })
            }
            State::AwaitScheduleExchangeRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::ScheduleExchangeRes(res) => Some(m.on_schedule_exchange_res(res, tx)),
                _ => None,
            }),
            State::AwaitCableCheckRes => self.dc_mainstream(payload_type, payload, arena, |m, msg| match msg {
                dc::Message::DcCableCheckRes(res) => Some(m.on_cable_check_res(res, tx)),
                _ => None,
            }),
            State::AwaitPreChargeRes => self.dc_mainstream(payload_type, payload, arena, |m, msg| match msg {
                dc::Message::DcPreChargeRes(res) => Some(m.on_precharge_res(res, tx)),
                _ => None,
            }),
            State::AwaitPowerDeliveryRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::PowerDeliveryRes(res) => Some(m.on_power_delivery_start_res(res, tx)),
                _ => None,
            }),
            State::AwaitChargeLoopRes if self.config.ac() => self.ac_mainstream(payload_type, payload, arena, |m, msg| match msg {
                ac::Message::AcChargeLoopRes(res) => Some(m.on_ac_charge_loop_res(res, tx)),
                _ => None,
            }),
            State::AwaitChargeLoopRes => self.dc_mainstream(payload_type, payload, arena, |m, msg| match msg {
                dc::Message::DcChargeLoopRes(res) => Some(m.on_charge_loop_res(res, tx)),
                _ => None,
            }),
            State::AwaitPowerDeliveryStandbyRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::PowerDeliveryRes(res) => Some(m.on_power_delivery_standby_res(res, tx)),
                _ => None,
            }),
            State::AwaitPowerDeliveryStopRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::PowerDeliveryRes(res) => Some(m.on_power_delivery_stop_res(res, tx)),
                _ => None,
            }),
            State::AwaitWeldingDetectionRes => self.dc_mainstream(payload_type, payload, arena, |m, msg| match msg {
                dc::Message::DcWeldingDetectionRes(res) => Some(m.on_welding_detection_res(res, tx)),
                _ => None,
            }),
            State::AwaitSessionStopRes => self.mainstream(payload_type, payload, arena, |m, msg| match msg {
                M::SessionStopRes(res) => Some(m.on_session_stop_res(res, tx)),
                _ => None,
            }),
            _ => self.ignore(payload_type),
        }
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
