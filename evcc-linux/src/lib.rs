//! Host adapter for std platforms: drives an [`evcc_core::session::Session`] over a blocking
//! `TcpStream`. The core's timer commands become socket read timeouts, so no thread or
//! async runtime is needed. (TLS, SDP sockets and SLAC are added later.)

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use evcc_core::{Timer, TimerCmd};
use evcc_core::ev::Ev;
use evcc_core::log::LogSink;
use evcc_core::session::{EndReason, Event, Notice, Session, Step};

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
pub fn run<E: Ev, L: LogSink>(
    stream: &mut TcpStream,
    session: &mut Session<'_, E, L>,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
) -> io::Result<Outcome> {
    match drive(stream, session, secured, host) {
        Err(e) if is_hang_up(&e) => Ok(Outcome::ConnectionClosed),
        other => other,
    }
}

fn is_hang_up(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
    )
}

fn drive<E: Ev, L: LogSink>(
    stream: &mut TcpStream,
    session: &mut Session<'_, E, L>,
    secured: bool,
    host: &mut impl FnMut(&Notice) -> Option<Event>,
) -> io::Result<Outcome> {
    let mut deadlines = Deadlines::default();
    let mut step = session.handle(Event::Start { secured });
    loop {
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
        match next_step(stream, session, &mut deadlines)? {
            Next::Step(next) => step = next,
            Next::PeerClosed => return Ok(Outcome::ConnectionClosed),
        }
    }
}

/// Pending expiry per timer.
#[derive(Default)]
struct Deadlines([Option<Instant>; 4]);

impl Deadlines {
    fn slot(timer: Timer) -> usize {
        match timer {
            Timer::Sdp => 0,
            Timer::Response => 1,
            Timer::Ongoing => 2,
            Timer::Poll => 3,
        }
    }

    fn apply(&mut self, cmd: TimerCmd) {
        match cmd {
            TimerCmd::Arm { timer, ms } => {
                self.0[Self::slot(timer)] = Some(Instant::now() + Duration::from_millis(u64::from(ms)));
            }
            TimerCmd::Cancel(timer) => self.0[Self::slot(timer)] = None,
        }
    }

    /// The earliest pending expiry and its timer.
    fn next(&self) -> Option<(Timer, Instant)> {
        [Timer::Sdp, Timer::Response, Timer::Ongoing, Timer::Poll]
            .into_iter()
            .filter_map(|t| self.0[Self::slot(t)].map(|d| (t, d)))
            .min_by_key(|&(_, d)| d)
    }

    fn clear(&mut self, timer: Timer) {
        self.0[Self::slot(timer)] = None;
    }
}

enum Next {
    Step(Step),
    PeerClosed,
}

/// Waits for the next socket data or timer expiry and feeds it to the session.
fn next_step<E: Ev, L: LogSink>(
    stream: &mut TcpStream,
    session: &mut Session<'_, E, L>,
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
            Some(left)
        }
        None => None,
    };
    stream.set_read_timeout(timeout)?;
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
