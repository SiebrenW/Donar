//! Host adapter for std platforms: drives an [`evcc_core::session::Session`] over a blocking
//! `TcpStream`. The core's timer commands become socket read timeouts, so no thread or
//! async runtime is needed. (TLS, SDP sockets and SLAC are added later.)

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use evcc_core::TimerCmd;
use evcc_core::ev::Ev;
use evcc_core::log::LogSink;
use evcc_core::session::{EndReason, Event, Notice, Session, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The part of the flow implemented so far finished: the session is set up.
    SessionSetUp { session_id: [u8; 8] },
    /// The session machine ended.
    Ended(EndReason),
    /// The peer closed the connection while the session was still running.
    ConnectionClosed,
}

/// Runs `session` over `stream` until it reaches a result. A peer that hangs up, cleanly or
/// by resetting the connection, is reported as [`Outcome::ConnectionClosed`]; other I/O
/// errors are returned.
pub fn run<E: Ev, L: LogSink>(stream: &mut TcpStream, session: &mut Session<'_, E, L>, secured: bool) -> io::Result<Outcome> {
    match drive(stream, session, secured) {
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

fn drive<E: Ev, L: LogSink>(stream: &mut TcpStream, session: &mut Session<'_, E, L>, secured: bool) -> io::Result<Outcome> {
    let mut deadline: Option<Instant> = None;
    let mut step = session.handle(Event::Start { secured });
    loop {
        if let Some(n) = step.send {
            stream.write_all(&session.tx()[..n])?;
        }
        match step.timer {
            TimerCmd::Arm { ms } => deadline = Some(Instant::now() + Duration::from_millis(u64::from(ms))),
            TimerCmd::Cancel => deadline = None,
            TimerCmd::Keep => {}
        }
        if let Some(reason) = step.end {
            return Ok(Outcome::Ended(reason));
        }
        if let Some(Notice::SessionSetUp { session_id }) = step.notice {
            return Ok(Outcome::SessionSetUp { session_id });
        }
        match next_step(stream, session, &mut deadline)? {
            Next::Step(next) => step = next,
            Next::PeerClosed => return Ok(Outcome::ConnectionClosed),
        }
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
    deadline: &mut Option<Instant>,
) -> io::Result<Next> {
    let timeout = match *deadline {
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left.is_zero() {
                *deadline = None;
                return Ok(Next::Step(session.handle(Event::TimerExpired)));
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
            *deadline = None;
            Ok(Next::Step(session.handle(Event::TimerExpired)))
        }
        Err(e) => Err(e),
    }
}
