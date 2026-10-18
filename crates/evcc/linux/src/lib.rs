//! Host adapter for std platforms: drives an [`evcc_core::session::Session`] over a blocking
//! `TcpStream`. The core's timer commands become socket read timeouts, so no thread or
//! async runtime is needed. The stream is a plain `TcpStream` or an OpenSSL TLS stream (see
//! [`tls`]). (SLAC is added later.)

#[cfg(target_os = "linux")]
pub mod slac;
#[cfg(feature = "tls")]
pub mod certificates;
#[cfg(feature = "tls")]
pub mod ocsp;
#[cfg(feature = "tls")]
pub mod pnc;
pub mod pilot;
pub mod sdp;
#[cfg(feature = "tls")]
pub mod tls;
#[cfg(feature = "tls")]
pub mod xmlsig;

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
#[cfg(feature = "tls")]
use openssl::ssl::SslStream;
use std::time::{Duration, Instant};

use evcc_core::{Timer, TimerCmd};
use evcc_core::machine::Machine;
use evcc_core::session::{EndReason, Event, Notice, State, Step};

/// A byte stream whose reads can time out, which is how the timer commands are served.
pub trait Transport: Read + Write {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

impl Transport for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}

#[cfg(feature = "tls")]
impl Transport for SslStream<TcpStream> {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.get_ref().set_read_timeout(timeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The session machine ended.
    Ended(EndReason),
    /// The peer closed the connection while the session was still running.
    ConnectionClosed,
}

/// Runs `session` over `stream` until it reaches a result. A peer that hangs up, cleanly or
/// by resetting the connection, is reported as [`Outcome::ConnectionClosed`]; other I/O
/// errors are returned.
pub fn run(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
) -> io::Result<Outcome> {
    run_gated(stream, session, secured, host, &mut |_| false)
}

/// Like [`run`], with a gate the host can hold a session at: `gate` is called with the state
/// the session just entered, before the frame of that step goes out or its timers start, and
/// may block (a test platform stepping through a session). When it returns true the time it
/// took is added to every pending timer, so a wait does not count against the protocol's
/// timeouts; with false the timers ran in real time.
pub fn run_gated(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
    gate: &mut impl FnMut(State) -> bool,
) -> io::Result<Outcome> {
    run_with_pilot(stream, session, secured, host, gate, None)
}

/// Like [`run_gated`], with a source of control pilot readings. The host waits for the socket in slices of
/// [`pilot::POLL_INTERVAL`] and reports every change as [`Event::Pilot`] (IEC 61851-23 asks the vehicle for
/// 50 ms to react to an emergency); when the session ends because of it ([`EndReason::Emergency`],
/// [`EndReason::PilotFault`]) the caller drops the connection without waiting for anything.
pub fn run_with_pilot(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
    gate: &mut impl FnMut(State) -> bool,
    mut pilot: Option<&mut dyn pilot::PilotSource>,
) -> io::Result<Outcome> {
    match drive(stream, session, secured, host, gate, &mut pilot) {
        Err(e) if is_hang_up(&e) => {
            // A charger that has an emergency turns the oscillator off and drops the connection: whether our write or our read
            // found it out first, the pilot is what tells an emergency from a hang-up.
            if let Some(reason) = pilot.as_mut().and_then(|p| p.poll()).and_then(|reading| session.handle(Event::Pilot(reading)).end) {
                return Ok(Outcome::Ended(reason));
            }
            Ok(Outcome::ConnectionClosed)
        }
        other => other,
    }
}

fn is_hang_up(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
    )
}

fn drive(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
    gate: &mut impl FnMut(State) -> bool,
    pilot: &mut Option<&mut dyn pilot::PilotSource>,
) -> io::Result<Outcome> {
    let mut deadlines = Deadlines::default();
    let mut step = session.handle(Event::Start { secured });
    loop {
        if step.end.is_none() {
            let held_from = Instant::now();
            if gate(session.state()) {
                deadlines.postpone(held_from.elapsed());
            }
        }
        if let (Some(guard), Some(source)) = (step.pilot, pilot.as_deref_mut()) {
            source.arm(guard);
        }
        if let Some(n) = step.send {
            stream.write_all(&session.tx()[..n])?;
        }
        for cmd in step.timers.iter() {
            deadlines.apply(cmd);
        }
        if let Some(reason) = step.end {
            return Ok(Outcome::Ended(reason));
        }
        if let Some(notice) = step.notice {
            // The host may act on a notice (close the contactors) and answer with an event.
            if let Some(event) = host(&notice) {
                step = session.handle(event);
                continue;
            }
        }
        match next_step(stream, session, &mut deadlines, pilot)? {
            Next::Step(next) => step = next,
            Next::PeerClosed => return Ok(Outcome::ConnectionClosed),
        }
    }
}

const ALL_TIMERS: [Timer; 11] = [
    Timer::Sdp,
    Timer::Response,
    Timer::Ongoing,
    Timer::Poll,
    Timer::Setup,
    Timer::Sequence,
    Timer::Side,
    Timer::Slac,
    Timer::SlacResults,
    Timer::SlacOverall,
    Timer::ReadyToCharge,
];

/// Pending expiry per timer.
#[derive(Default)]
pub(crate) struct Deadlines([Option<Instant>; 11]);

impl Deadlines {
    pub(crate) fn slot(timer: Timer) -> usize {
        match timer {
            Timer::Sdp => 0,
            Timer::Response => 1,
            Timer::Ongoing => 2,
            Timer::Poll => 3,
            Timer::Setup => 4,
            Timer::Sequence => 5,
            Timer::Slac => 6,
            Timer::SlacResults => 7,
            Timer::SlacOverall => 8,
            Timer::Side => 9,
            Timer::ReadyToCharge => 10,
        }
    }

    pub(crate) fn apply(&mut self, cmd: TimerCmd) {
        match cmd {
            TimerCmd::Arm { timer, ms } => {
                self.0[Self::slot(timer)] = Some(Instant::now() + Duration::from_millis(u64::from(ms)));
            }
            TimerCmd::Cancel(timer) => self.0[Self::slot(timer)] = None,
        }
    }

    /// Moves every pending expiry later by `by`.
    pub(crate) fn postpone(&mut self, by: Duration) {
        for d in self.0.iter_mut().flatten() {
            *d += by;
        }
    }

    /// The earliest pending expiry and its timer.
    pub(crate) fn next(&self) -> Option<(Timer, Instant)> {
        ALL_TIMERS
            .into_iter()
            .filter_map(|t| self.0[Self::slot(t)].map(|d| (t, d)))
            .min_by_key(|&(_, d)| d)
    }

    pub(crate) fn clear(&mut self, timer: Timer) {
        self.0[Self::slot(timer)] = None;
    }
}

enum Next {
    Step(Step),
    PeerClosed,
}

/// Waits for the next socket data or timer expiry and feeds it to the session.
fn next_step(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    deadlines: &mut Deadlines,
    pilot: &mut Option<&mut dyn pilot::PilotSource>,
) -> io::Result<Next> {
    match pilot {
        None => next_step_plain(stream, session, deadlines),
        Some(pilot) => next_step_with_pilot(stream, session, deadlines, &mut **pilot),
    }
}

/// As [`next_step_plain`], but the wait is cut into slices of [`pilot::POLL_INTERVAL`] and the pilot is looked at
/// between them.
fn next_step_with_pilot(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    deadlines: &mut Deadlines,
    pilot: &mut dyn pilot::PilotSource,
) -> io::Result<Next> {
    loop {
        if let Some(reading) = pilot.poll() {
            return Ok(Next::Step(session.handle(Event::Pilot(reading))));
        }
        let mut timeout = pilot::POLL_INTERVAL;
        if let Some((timer, d)) = deadlines.next() {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                deadlines.clear(timer);
                return Ok(Next::Step(session.handle(Event::TimerExpired(timer))));
            }
            timeout = timeout.min(left.max(Duration::from_millis(1)));
        }
        match stream.set_read_timeout(Some(timeout)) {
            Err(e) if e.kind() == ErrorKind::InvalidInput => return Err(ErrorKind::ConnectionAborted.into()),
            other => other?,
        }
        let space = session.rx_space();
        if space.is_empty() {
            return Err(io::Error::new(ErrorKind::OutOfMemory, "RX buffer full"));
        }
        match stream.read(space) {
            Ok(0) => {
                // The charger that has an emergency turns the oscillator off and may drop the connection at once: the pilot
                // is what tells an emergency from a hang-up.
                if let Some(reading) = pilot.poll() {
                    return Ok(Next::Step(session.handle(Event::Pilot(reading))));
                }
                return Ok(Next::PeerClosed);
            }
            Err(e) if is_hang_up(&e) => {
                if let Some(reading) = pilot.poll() {
                    return Ok(Next::Step(session.handle(Event::Pilot(reading))));
                }
                return Err(e);
            }
            Ok(n) => return Ok(Next::Step(session.handle(Event::Received(n)))),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        }
    }
}

fn next_step_plain(
    stream: &mut impl Transport,
    session: &mut impl Machine,
    deadlines: &mut Deadlines,
) -> io::Result<Next> {
    let pending = deadlines.next();
    let timeout = match pending {
        Some((timer, d)) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                deadlines.clear(timer);
                return Ok(Next::Step(session.handle(Event::TimerExpired(timer))));
            }
            // A zero (or sub-millisecond) timeout is invalid for a socket.
            Some(left.max(Duration::from_millis(1)))
        }
        None => None,
    };
    // macOS refuses to set an option on a socket the peer has already closed (EINVAL); that is
    // a hang-up like any other.
    match stream.set_read_timeout(timeout) {
        Err(e) if e.kind() == ErrorKind::InvalidInput => return Err(ErrorKind::ConnectionAborted.into()),
        other => other?,
    }
    let space = session.rx_space();
    if space.is_empty() {
        return Err(io::Error::new(ErrorKind::OutOfMemory, "RX buffer full"));
    }
    match stream.read(space) {
        Ok(0) => Ok(Next::PeerClosed),
        Ok(n) => Ok(Next::Step(session.handle(Event::Received(n)))),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            // The read timed out; the earliest timer is the one that fired.
            match pending {
                Some((timer, _)) => {
                    deadlines.clear(timer);
                    Ok(Next::Step(session.handle(Event::TimerExpired(timer))))
                }
                None => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}
