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

