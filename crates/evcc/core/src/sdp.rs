//! SECC Discovery client: finds the charger's address, port and security mode.
//!
//! The host sends the datagram the machine produces to `v2gtp::sdp::MULTICAST_ADDR` port
//! `v2gtp::sdp::SDP_PORT` on the charging interface, feeds back datagrams that arrive on the
//! same socket, and fires [`Event::TimerExpired`] when the armed timer elapses.
//!
//! Timing (ISO 15118-20:2022): wait at least 250 ms for a response (V2G20-159), then send a
//! new request (V2G20-160); stop after at most 50 consecutive requests (V2G20-161) and treat
//! that as a timeout (V2G20-162). Both values are configurable in [`Config`]. Only a response
//! Wireless communication (ISO 15118-8, [`Config::wireless`]): the request carries the vehicle's EVID, the pairing and
//! positioning device it uses and the coupling type; the charger answers `Ongoing` until its device has detected the EVID
//! (V2G20-4102), which resets the count of requests (V2G20-4101) and is followed by the next request no sooner than
//! 250 ms later (V2G20-2288), and `Finished` with the address when it has. Only wireless responses count then (V2G20-2278).
//! Only a response
//! with TCP and TLS makes -20 usable (V2G20-1804, V2G20-1805), hence `accept_other_security`
//! is off by default.

use v2gtp::sdp::{DiagStatus, EvId, EvseId, Request, Response, Security, Transport, WirelessRequest, WirelessResponse};
use v2gtp::{BufferTooSmall, Parse, PayloadType, parse};

use crate::{Timer, TimerCmd, Timers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Wireless discovery (ISO 15118-8) instead of the wired one.
    pub wireless: Option<Wireless>,
    /// Security mode the EVCC asks for.
    pub security: Security,
    /// Accept a response whose security differs from the request. Off by default: an EV
    /// that asked for TLS should not silently fall back to plain TCP.
    pub accept_other_security: bool,
    /// Accept a charger address that is not on the link. Off by default: the charger is found by a multicast on the cable and
    /// uses a link-local address (or a unique local one), so a response that names a global address (or the loopback)
    /// is forged or wrong, and the vehicle would connect to it through whatever route it has.
    pub accept_other_address: bool,
    pub request_interval_ms: u32,
    pub max_requests: u8,
}

/// What a wireless request says about the vehicle (ISO 15118-20 Tables 22 to 25).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wireless {
    /// The P2PS/PPD bytes (`v2gtp::sdp::p2ps`): bit 0 of the first says the pairing and positioning device is on the
    /// infrastructure (V2G20-4003, 4100).
    pub p2ps_ppd: [u8; 2],
    /// `v2gtp::sdp::coupling`: V2G20-4003 sets the in line contacts of EN 50696 Annex A.3 for ACDP.
    pub coupling_type: u8,
    /// "ZZ00000" for a vehicle that has none (V2G20-2281).
    pub evid: EvId,
    /// Empty when the vehicle uses a PPD on the infrastructure (V2G20-4100).
    pub evse_id: EvseId,
}

impl Config {
    pub const fn new(security: Security) -> Self {
        Self { wireless: None, security, accept_other_security: false, accept_other_address: false, request_interval_ms: 250, max_requests: 50 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// Begin (or restart) discovery.
    Start,
    TimerExpired,
    /// A UDP datagram received on the SDP socket.
    Datagram(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Found(Response),
    /// No acceptable response after `max_requests` requests.
    NoResponse,
}

/// Result of one step. Fields are independent; the host handles all that are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Number of bytes at the start of the `tx` buffer to send as a multicast datagram.
    pub send: Option<usize>,
    pub timers: Timers,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Waiting { sent: u8 },
    Done,
}

pub struct Client {
    config: Config,
    state: State,
    /// The EVSEID of the charger that answered a wireless request with `FinishedWithEvseId`.
    evse_id: Option<EvseId>,
}

/// What a datagram was.
enum Heard {
    Found(Response),
    /// A wireless `Ongoing`: the charger is there and has not detected the vehicle yet.
    Ongoing,
    Nothing,
}

impl Client {
    pub const fn new(config: Config) -> Self {
        Self { config, state: State::Idle, evse_id: None }
    }

    /// The EVSEID the charger gave in its wireless answer, if it did.
    pub fn evse_id(&self) -> Option<EvseId> {
        self.evse_id
    }

    pub fn handle(&mut self, event: Event<'_>, tx: &mut [u8]) -> Result<Step, BufferTooSmall> {
        match (self.state, event) {
            (_, Event::Start) => self.send_request(1, tx),
            (State::Waiting { sent }, Event::TimerExpired) => {
                if sent >= self.config.max_requests {
                    self.state = State::Done;
                    Ok(Step { outcome: Some(Outcome::NoResponse), ..Step::default() })
                } else {
                    self.send_request(sent + 1, tx)
                }
            }
            (State::Waiting { .. }, Event::Datagram(bytes)) => Ok(match self.hear(bytes) {
                Heard::Found(response) => {
                    self.state = State::Done;
                    Step { timers: Timers::one(TimerCmd::Cancel(Timer::Sdp)), outcome: Some(Outcome::Found(response)), ..Step::default() }
                }
                // The charger is there: a new count, and the next request no sooner than the interval from now.
                Heard::Ongoing => {
                    self.state = State::Waiting { sent: 0 };
                    Step { timers: Timers::one(TimerCmd::Arm { timer: Timer::Sdp, ms: self.config.request_interval_ms }), ..Step::default() }
                }
                // Garbage, wrong type or unacceptable security: keep waiting for the real one.
                Heard::Nothing => Step::default(),
            }),
            // Stale timer or datagram while idle/done.
            _ => Ok(Step::default()),
        }
    }

    fn send_request(&mut self, count: u8, tx: &mut [u8]) -> Result<Step, BufferTooSmall> {
        let len = match self.config.wireless {
            None => Request { security: self.config.security, transport: Transport::Tcp }.to_datagram(tx)?,
            Some(w) => WirelessRequest {
                security: self.config.security,
                transport: Transport::Tcp,
                p2ps_ppd: w.p2ps_ppd,
                coupling_type: w.coupling_type,
                evid: w.evid,
                evse_id: w.evse_id,
            }
            .to_datagram(tx)?,
        };
        self.state = State::Waiting { sent: count };
        Ok(Step {
            send: Some(len),
            timers: Timers::one(TimerCmd::Arm { timer: Timer::Sdp, ms: self.config.request_interval_ms }),
            outcome: None,
        })
    }

    fn hear(&mut self, bytes: &[u8]) -> Heard {
        let Some(wireless) = self.config.wireless else {
            return self.accept(bytes).map_or(Heard::Nothing, Heard::Found);
        };
        let Ok(Parse::Frame(frame)) = parse(bytes, WirelessResponse::PAYLOAD_LEN) else { return Heard::Nothing };
        if frame.payload_type != PayloadType::SDP_RESPONSE_WIRELESS {
            return Heard::Nothing;
        }
        let Ok(response) = WirelessResponse::from_payload(frame.payload) else { return Heard::Nothing };
        // A charger of another coupling type has no business answering (V2G20-2290 and the text before 2293).
        if response.coupling_type != wireless.coupling_type {
            return Heard::Nothing;
        }
        match response.diag_status {
            DiagStatus::Ongoing => Heard::Ongoing,
            DiagStatus::ErrorNoSecc => Heard::Nothing,
            DiagStatus::FinishedWithEvseId | DiagStatus::FinishedWithoutEvseId => {
                if !self.address_usable(&response.as_response()) {
                    return Heard::Nothing;
                }
                self.evse_id = (response.diag_status == DiagStatus::FinishedWithEvseId).then_some(response.evse_id);
                Heard::Found(response.as_response())
            }
        }
    }

    fn address_usable(&self, response: &Response) -> bool {
        let security_ok = response.security == self.config.security || self.config.accept_other_security;
        // UDP is only allowed for SDP itself, never for V2G messages.
        let address_ok = is_on_link(response.address) || self.config.accept_other_address;
        response.transport == Transport::Tcp && response.port != 0 && security_ok && address_ok
    }

    fn accept(&self, bytes: &[u8]) -> Option<Response> {
        let Ok(Parse::Frame(frame)) = parse(bytes, Response::PAYLOAD_LEN) else { return None };
        if frame.payload_type != PayloadType::SDP_RESPONSE {
            return None;
        }
        let response = Response::from_payload(frame.payload).ok()?;
        self.address_usable(&response).then_some(response)
    }
}

/// A link-local (`fe80::/10`) or unique local (`fc00::/7`) unicast address: one that is reached over the cable.
fn is_on_link(address: [u8; 16]) -> bool {
    (address[0] == 0xfe && address[1] & 0xc0 == 0x80) || address[0] & 0xfe == 0xfc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(security: Security) -> Response {
        Response {
            address: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 50000,
            security,
            transport: Transport::Tcp,
        }
    }

    fn datagram(r: &Response) -> ([u8; 32], usize) {
        let mut buf = [0u8; 32];
        let n = r.to_datagram(&mut buf).unwrap();
        (buf, n)
    }

    #[test]
    fn only_an_address_on_the_link_is_accepted() {
        let mut tx = [0u8; 16];
        let mut try_address = |address: [u8; 16], accept_other_address: bool| {
            let mut c = Client::new(Config { accept_other_address, ..Config::new(Security::Tls) });
            c.handle(Event::Start, &mut tx).unwrap();
            let (buf, n) = datagram(&Response { address, ..response(Security::Tls) });
            c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap().outcome.is_some()
        };
        let mut address = [0u8; 16];
        let mut with = |first: u8, second: u8| {
            address[0] = first;
            address[1] = second;
            address[15] = 1;
            address
        };
        assert!(try_address(with(0xfe, 0x80), false), "link-local");
        assert!(try_address(with(0xfe, 0xbf), false), "the end of fe80::/10");
        assert!(try_address(with(0xfd, 0x12), false), "unique local");
        assert!(!try_address(with(0xfe, 0xc0), false), "site-local is not on the link");
        assert!(!try_address(with(0x20, 0x01), false), "global");
        assert!(!try_address(with(0xff, 0x02), false), "multicast");
        let mut loopback = [0u8; 16];
        loopback[15] = 1;
        assert!(!try_address(loopback, false), "loopback");
        assert!(try_address(loopback, true), "unless the host says so (a test on one machine)");
        assert!(try_address(with(0x20, 0x01), true));
    }

    #[test]
    fn first_request_and_timer() {
        let mut c = Client::new(Config::new(Security::Tls));
        let mut tx = [0u8; 16];
        let step = c.handle(Event::Start, &mut tx).unwrap();
        assert_eq!(step.send, Some(10));
        assert_eq!(&tx[..10], &[0x01, 0xFE, 0x90, 0x00, 0, 0, 0, 2, 0x00, 0x00]);
        assert_eq!(step.timers, Timers::one(TimerCmd::Arm { timer: Timer::Sdp, ms: 250 }));
        assert_eq!(step.outcome, None);
    }

    #[test]
    fn retries_then_gives_up_after_exactly_max_requests() {
        let mut c = Client::new(Config::new(Security::NoTls));
        let mut tx = [0u8; 16];
        let mut sends = 1; // Start
        assert!(c.handle(Event::Start, &mut tx).unwrap().send.is_some());
        let outcome = loop {
            let step = c.handle(Event::TimerExpired, &mut tx).unwrap();
            if step.send.is_some() {
                sends += 1;
                assert_eq!(step.timers, Timers::one(TimerCmd::Arm { timer: Timer::Sdp, ms: 250 }));
            }
            if let Some(o) = step.outcome {
                break o;
            }
        };
        assert_eq!(sends, 50);
        assert_eq!(outcome, Outcome::NoResponse);
        // Finished: further timer events do nothing.
        assert_eq!(c.handle(Event::TimerExpired, &mut tx).unwrap(), Step::default());
    }

    #[test]
    fn matching_response_ends_discovery_and_cancels_timer() {
        let mut c = Client::new(Config::new(Security::Tls));
        let mut tx = [0u8; 16];
        c.handle(Event::Start, &mut tx).unwrap();
        let r = response(Security::Tls);
        let (buf, n) = datagram(&r);
        let step = c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap();
        assert_eq!(step.outcome, Some(Outcome::Found(r)));
        assert_eq!(step.timers, Timers::one(TimerCmd::Cancel(Timer::Sdp)));
        // A late duplicate is ignored.
        assert_eq!(c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap(), Step::default());
    }

    #[test]
    fn unacceptable_datagrams_are_ignored_and_the_timer_keeps_running() {
        let mut c = Client::new(Config::new(Security::Tls));
        let mut tx = [0u8; 16];
        c.handle(Event::Start, &mut tx).unwrap();
        let (other, n) = datagram(&response(Security::NoTls)); // wrong security
        let mut udp = response(Security::Tls);
        udp.transport = Transport::Udp;
        let (udp, un) = datagram(&udp);
        let mut port0 = response(Security::Tls);
        port0.port = 0;
        let (port0, pn) = datagram(&port0);
        let mut request_echo = [0u8; 16];
        let rn = Request { security: Security::Tls, transport: Transport::Tcp }.to_datagram(&mut request_echo).unwrap();
        for junk in [&other[..n], &udp[..un], &port0[..pn], &request_echo[..rn], &[][..], &[0xff; 40][..], &other[..n - 1]] {
            assert_eq!(c.handle(Event::Datagram(junk), &mut tx).unwrap(), Step::default());
        }
        // Still waiting: the next timer expiry retransmits.
        assert!(c.handle(Event::TimerExpired, &mut tx).unwrap().send.is_some());
    }

    #[test]
    fn security_fallback_only_when_allowed() {
        let mut cfg = Config::new(Security::Tls);
        cfg.accept_other_security = true;
        let mut c = Client::new(cfg);
        let mut tx = [0u8; 16];
        c.handle(Event::Start, &mut tx).unwrap();
        let (buf, n) = datagram(&response(Security::NoTls));
        let step = c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap();
        assert!(matches!(step.outcome, Some(Outcome::Found(r)) if r.security == Security::NoTls));
    }

    #[test]
    fn restart_begins_a_new_cycle() {
        let mut c = Client::new(Config { max_requests: 1, ..Config::new(Security::Tls) });
        let mut tx = [0u8; 16];
        c.handle(Event::Start, &mut tx).unwrap();
        assert_eq!(c.handle(Event::TimerExpired, &mut tx).unwrap().outcome, Some(Outcome::NoResponse));
        assert!(c.handle(Event::Start, &mut tx).unwrap().send.is_some());
    }

    #[test]
    fn small_tx_buffer_is_reported() {
        let mut c = Client::new(Config::new(Security::Tls));
        assert_eq!(c.handle(Event::Start, &mut [0u8; 4]), Err(BufferTooSmall));
    }

    fn wireless_config() -> Config {
        let wireless = Wireless {
            p2ps_ppd: [v2gtp::sdp::p2ps::PPD_ON_INFRASTRUCTURE, 0],
            coupling_type: v2gtp::sdp::coupling::PANTOGRAPH_IN_LINE,
            evid: EvId::new("BUS42").unwrap(),
            evse_id: EvseId::EMPTY,
        };
        Config { wireless: Some(wireless), ..Config::new(Security::Tls) }
    }

    fn wireless_answer(diag_status: DiagStatus, coupling_type: u8) -> ([u8; 80], usize) {
        let r = WirelessResponse {
            address: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            port: 50000,
            security: Security::Tls,
            transport: Transport::Tcp,
            diag_status,
            coupling_type,
            evse_id: EvseId::new("DE9XYZ1").unwrap(),
        };
        let mut buf = [0u8; 80];
        let n = r.to_datagram(&mut buf).unwrap();
        (buf, n)
    }

    #[test]
    fn a_wireless_client_sends_the_wireless_request() {
        let mut c = Client::new(wireless_config());
        let mut tx = [0u8; 80];
        let step = c.handle(Event::Start, &mut tx).unwrap();
        assert_eq!(step.send, Some(70));
        assert_eq!(&tx[..8], &[0x01, 0xFE, 0x90, 0x02, 0, 0, 0, 62]);
        assert_eq!(WirelessRequest::from_payload(&tx[8..70]).unwrap().evid.as_str(), "BUS42");
        assert_eq!(c.handle(Event::Start, &mut [0u8; 40]), Err(BufferTooSmall));
    }

    #[test]
    fn ongoing_resets_the_count_and_restarts_the_wait_and_finished_gives_the_address() {
        let coupling = v2gtp::sdp::coupling::PANTOGRAPH_IN_LINE;
        let mut c = Client::new(Config { max_requests: 3, ..wireless_config() });
        let mut tx = [0u8; 80];
        c.handle(Event::Start, &mut tx).unwrap();
        // Two requests go unanswered, an Ongoing answer arrives: the count starts again and the interval from now.
        c.handle(Event::TimerExpired, &mut tx).unwrap();
        let (buf, n) = wireless_answer(DiagStatus::Ongoing, coupling);
        let step = c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap();
        assert_eq!(step.timers, Timers::one(TimerCmd::Arm { timer: Timer::Sdp, ms: 250 }));
        assert_eq!(step.outcome, None);
        for _ in 0..2 {
            let step = c.handle(Event::TimerExpired, &mut tx).unwrap();
            assert!(step.send.is_some() && step.outcome.is_none(), "three requests again before it gives up");
        }
        let (buf, n) = wireless_answer(DiagStatus::FinishedWithEvseId, coupling);
        let step = c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap();
        assert!(matches!(step.outcome, Some(Outcome::Found(r)) if r.port == 50000));
        assert_eq!(c.evse_id().unwrap().as_str(), "DE9XYZ1");
    }

    #[test]
    fn finished_without_an_evseid_leaves_it_out_and_the_wrong_answers_are_ignored() {
        let mut c = Client::new(wireless_config());
        let mut tx = [0u8; 80];
        c.handle(Event::Start, &mut tx).unwrap();
        let coupling = v2gtp::sdp::coupling::PANTOGRAPH_IN_LINE;
        let (wrong_coupling, a) = wireless_answer(DiagStatus::FinishedWithEvseId, v2gtp::sdp::coupling::WPT);
        let (no_secc, b) = wireless_answer(DiagStatus::ErrorNoSecc, coupling);
        let (wired, w) = datagram(&response(Security::Tls)); // V2G20-2278: only wireless responses
        for junk in [&wrong_coupling[..a], &no_secc[..b], &wired[..w]] {
            assert_eq!(c.handle(Event::Datagram(junk), &mut tx).unwrap(), Step::default());
        }
        let (buf, n) = wireless_answer(DiagStatus::FinishedWithoutEvseId, coupling);
        assert!(c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap().outcome.is_some());
        assert_eq!(c.evse_id(), None);
    }

    #[test]
    fn a_wired_client_ignores_a_wireless_answer() {
        let mut c = Client::new(Config::new(Security::Tls));
        let mut tx = [0u8; 16];
        c.handle(Event::Start, &mut tx).unwrap();
        let (buf, n) = wireless_answer(DiagStatus::FinishedWithEvseId, v2gtp::sdp::coupling::PANTOGRAPH_IN_LINE);
        assert_eq!(c.handle(Event::Datagram(&buf[..n]), &mut tx).unwrap(), Step::default());
    }
}
