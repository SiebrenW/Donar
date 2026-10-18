//! Blocking network servers around the simulator, for end-to-end tests.

use std::io::{Read, Write};
use std::net::TcpListener;

use v2gtp::{Parse, PayloadType, parse};

use crate::{Config, Evse, State};

/// Accepts one connection and serves it until it closes. Blocking.
pub fn serve_one(listener: &TcpListener, config: Config) -> std::io::Result<State> {
    let (mut stream, _) = listener.accept()?;
    serve(&mut stream, config)
}

/// Serves one connection on any byte stream (a TLS stream, say) until it closes. Blocking.
pub fn serve(stream: &mut (impl Read + Write), config: Config) -> std::io::Result<State> {
    serve_evse(stream, Evse::new(config), None)
}

/// [`serve`] for a test platform: the live state sees the session and edits its measurements.
/// The config of the session is the one the live state holds for it.
pub fn serve_live(stream: &mut (impl Read + Write), live: &std::sync::Arc<crate::EvseLive>) -> std::io::Result<State> {
    let evse = Evse::new(live.begin_session()).with_live(live.clone());
    serve_evse(stream, evse, Some(live))
}

fn serve_evse(stream: &mut (impl Read + Write), mut evse: Evse, live: Option<&std::sync::Arc<crate::EvseLive>>) -> std::io::Result<State> {
    if let Some(live) = live {
        live.set_state(evse.state());
    }
    let mut buf = [0u8; 2048];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(evse.state());
        }
        let outputs = evse.handle(&buf[..n]);
        for line in evse.take_log() {
            eprintln!("[evse] {line}");
            if let Some(live) = live {
                live.event(&line);
            }
        }
        if let Some(live) = live {
            live.set_state(evse.state());
        }
        for out in outputs {
            if let Some(bytes) = out.send {
                stream.write_all(&bytes)?;
            }
            if out.close {
                return Ok(evse.state());
            }
        }
    }
}

/// [`serve_live`] (or, without a live state, [`serve`]) with the control pilot: the charger reads the vehicle's state from `wire`
/// and tells the session machine at once, also while it waits for the socket (IEC 61851-23 asks the charger for 10 ms),
/// and judges every request by what the pilot showed before it came. After `pilot_emergency_after_loops` charge loop
/// requests the charger has an emergency of its own: its oscillator goes off and the connection is dropped without a word.
pub fn serve_wired<T: evcc_linux::Transport>(stream: &mut T, config: crate::Config, live: Option<&std::sync::Arc<crate::EvseLive>>, wire: &evcc_linux::pilot::Wire) -> std::io::Result<State> {
    use evcc_linux::pilot::PilotSource;
    let mut evse = match live {
        Some(live) => Evse::new(live.begin_session()).with_live(live.clone()),
        None => Evse::new(config),
    };
    if let Some(live) = live {
        live.set_state(evse.state());
    }
    let mut source = wire.source();
    let mut buf = [0u8; 2048];
    // Writes what the machine answered; true when the session is over.
    fn emit<W: Write>(stream: &mut W, evse: &mut Evse, outputs: Vec<crate::Output>, live: Option<&std::sync::Arc<crate::EvseLive>>) -> std::io::Result<bool> {
        for line in evse.take_log() {
            eprintln!("[evse] {line}");
            if let Some(live) = live {
                live.event(&line);
            }
        }
        if let Some(live) = live {
            live.set_state(evse.state());
        }
        for out in outputs {
            if let Some(bytes) = out.send {
                stream.write_all(&bytes)?;
            }
            if out.close {
                return Ok(true);
            }
        }
        Ok(false)
    }
    loop {
        if let Some(reading) = source.poll() {
            let outputs = evse.pilot(reading);
            if emit(stream, &mut evse, outputs, live)? {
                return Ok(evse.state());
            }
        }
        stream.set_read_timeout(Some(evcc_linux::pilot::POLL_INTERVAL))?;
        match stream.read(&mut buf) {
            Ok(0) => {
                // A vehicle that has an emergency drops the connection at once: what it did to the pilot comes first.
                if let Some(reading) = source.poll() {
                    let outputs = evse.pilot(reading);
                    emit(stream, &mut evse, outputs, live)?;
                }
                return Ok(evse.state());
            }
            Ok(n) => {
                // What the pilot showed before the request came is what it is judged by.
                if let Some(reading) = source.poll() {
                    let outputs = evse.pilot(reading);
                    if emit(stream, &mut evse, outputs, live)? {
                        return Ok(evse.state());
                    }
                }
                let outputs = evse.handle(&buf[..n]);
                if emit(stream, &mut evse, outputs, live)? {
                    return Ok(evse.state());
                }
                if evse.pilot_emergency_due() {
                    eprintln!("[evse] an emergency of its own: the oscillator goes off, the connection is dropped");
                    wire.set_evse(control_pilot::Pwm::Off)?;
                    return Ok(State::Closed);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Answers SECC discovery requests on `socket` (blocking): the first `ignore` requests get no
/// answer, then one request is answered with `response` sent back to the requester. Returns
/// the number of requests seen, including the answered one.
pub fn answer_discovery(
    socket: &std::net::UdpSocket,
    response: v2gtp::sdp::Response,
    ignore: u32,
) -> std::io::Result<u32> {
    let mut buf = [0u8; 64];
    let mut seen = 0;
    loop {
        let (n, from) = socket.recv_from(&mut buf)?;
        let Ok(Parse::Frame(frame)) = parse(&buf[..n], v2gtp::sdp::Request::PAYLOAD_LEN) else { continue };
        if frame.payload_type != PayloadType::SDP_REQUEST || v2gtp::sdp::Request::from_payload(frame.payload).is_err() {
            continue;
        }
        seen += 1;
        if seen > ignore {
            let mut out = [0u8; 32];
            let len = response.to_datagram(&mut out).expect("buffer fits a response");
            socket.send_to(&out[..len], from)?;
            return Ok(seen);
        }
    }
}

