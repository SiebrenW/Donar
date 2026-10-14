//! SECC Discovery client: finds the charger's address, port and security mode.
//!
//! The host sends the datagram the machine produces to `v2gtp::sdp::MULTICAST_ADDR` port
//! `v2gtp::sdp::SDP_PORT` on the charging interface, feeds back datagrams that arrive on the
//! same socket, and fires [`Event::TimerExpired`] when the armed timer elapses.
//!
//! Timing (250 ms between requests, at most 50 requests per cycle) follows the values used
//! by open-source stacks for ISO 15118-2/-20; they are configurable in [`Config`].

use v2gtp::sdp::{Request, Response, Security, Transport};
use v2gtp::{BufferTooSmall, Parse, PayloadType, parse};

use crate::TimerCmd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Security mode the EVCC asks for.
    pub security: Security,
    /// Accept a response whose security differs from the request. Off by default: an EV
    /// that asked for TLS should not silently fall back to plain TCP.
    pub accept_other_security: bool,
    pub request_interval_ms: u32,
    pub max_requests: u8,
}

impl Config {
    pub const fn new(security: Security) -> Self {
        Self { security, accept_other_security: false, request_interval_ms: 250, max_requests: 50 }
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
    pub timer: TimerCmd,
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
}

impl Client {
    pub const fn new(config: Config) -> Self {
        Self { config, state: State::Idle }
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
            (State::Waiting { .. }, Event::Datagram(bytes)) => Ok(match self.accept(bytes) {
                Some(response) => {
                    self.state = State::Done;
                    Step { timer: TimerCmd::Cancel, outcome: Some(Outcome::Found(response)), ..Step::default() }
                }
                // Garbage, wrong type or unacceptable security: keep waiting for the real one.
                None => Step::default(),
            }),
            // Stale timer or datagram while idle/done.
            _ => Ok(Step::default()),
        }
    }

    fn send_request(&mut self, count: u8, tx: &mut [u8]) -> Result<Step, BufferTooSmall> {
        let request = Request { security: self.config.security, transport: Transport::Tcp };
        let len = request.to_datagram(tx)?;
        self.state = State::Waiting { sent: count };
        Ok(Step { send: Some(len), timer: TimerCmd::Arm { ms: self.config.request_interval_ms }, outcome: None })
    }

    fn accept(&self, bytes: &[u8]) -> Option<Response> {
        let Ok(Parse::Frame(frame)) = parse(bytes, Response::PAYLOAD_LEN) else { return None };
        if frame.payload_type != PayloadType::SDP_RESPONSE {
            return None;
        }
        let response = Response::from_payload(frame.payload).ok()?;
        let security_ok = response.security == self.config.security || self.config.accept_other_security;
        // UDP is only allowed for SDP itself, never for V2G messages.
        (response.transport == Transport::Tcp && response.port != 0 && security_ok).then_some(response)
    }
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
    fn first_request_and_timer() {
        let mut c = Client::new(Config::new(Security::Tls));
        let mut tx = [0u8; 16];
        let step = c.handle(Event::Start, &mut tx).unwrap();
        assert_eq!(step.send, Some(10));
        assert_eq!(&tx[..10], &[0x01, 0xFE, 0x90, 0x00, 0, 0, 0, 2, 0x00, 0x00]);
        assert_eq!(step.timer, TimerCmd::Arm { ms: 250 });
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
                assert_eq!(step.timer, TimerCmd::Arm { ms: 250 });
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
        assert_eq!(step.timer, TimerCmd::Cancel);
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
}
