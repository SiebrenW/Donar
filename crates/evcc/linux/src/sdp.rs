//! SECC discovery over UDP (ISO 15118-20:2022, 7.10): drives [`evcc_core::sdp::Client`] with a
//! UDP socket, and connects to the charger it finds.

use std::io::{self, ErrorKind};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use evcc_core::TimerCmd;
use evcc_core::sdp::{Client, Config, Event, Outcome, Step};
use v2gtp::sdp::{MULTICAST_ADDR, Response, SDP_PORT};

/// The index of a network interface, such as `eth1`.
pub fn interface_index(name: &str) -> io::Result<u32> {
    let c = std::ffi::CString::new(name).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "interface name"))?;
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => Err(io::Error::last_os_error()),
        index => Ok(index),
    }
}

/// Where the discovery request goes: the all-nodes link-local multicast group on `interface`
/// (V2G20 SDP uses `FF02::1` and UDP port 15118).
pub fn multicast_destination(interface: u32) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(MULTICAST_ADDR), SDP_PORT, 0, interface))
}

/// A socket for discovery: any local address and port; responses come back to it as unicast.
pub fn socket() -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
}

/// Sends discovery requests to `destination` and waits for a response, following the timing in
/// `config` (request interval, number of requests). Blocking.
pub fn discover(socket: &UdpSocket, destination: SocketAddr, config: Config) -> io::Result<Outcome> {
    let mut client = Client::new(config);
    let mut tx = [0u8; 80];
    let mut rx = [0u8; 128];
    let mut deadline: Option<Instant> = None;
    let mut event = Event::Start;
    loop {
        let step = client.handle(event, &mut tx).map_err(|_| io::Error::other("SDP buffer too small"))?;
        apply(socket, destination, &tx, &step, &mut deadline)?;
        if let Some(outcome) = step.outcome {
            return Ok(outcome);
        }
        let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if left == Some(Duration::ZERO) {
            deadline = None;
            event = Event::TimerExpired;
            continue;
        }
        // A zero (or sub-millisecond) timeout is invalid for a socket.
        socket.set_read_timeout(left.map(|l| l.max(Duration::from_millis(1))))?;
        match socket.recv(&mut rx) {
            Ok(n) => event = Event::Datagram(&rx[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                deadline = None;
                event = Event::TimerExpired;
            }
            Err(e) => return Err(e),
        }
    }
}

fn apply(
    socket: &UdpSocket,
    destination: SocketAddr,
    tx: &[u8],
    step: &Step,
    deadline: &mut Option<Instant>,
) -> io::Result<()> {
    if let Some(n) = step.send {
        socket.send_to(&tx[..n], destination)?;
    }
    for cmd in step.timers.iter() {
        match cmd {
            TimerCmd::Arm { ms, .. } => *deadline = Some(Instant::now() + Duration::from_millis(u64::from(ms))),
            TimerCmd::Cancel(_) => *deadline = None,
        }
    }
    Ok(())
}

/// The socket address of the charger a response describes. A link-local address needs the
/// interface it was found on as its scope.
pub fn address(response: &Response, interface: u32) -> SocketAddrV6 {
    SocketAddrV6::new(Ipv6Addr::from(response.address), response.port, 0, interface)
}

/// Opens the TCP connection to the charger.
pub fn connect(response: &Response, interface: u32) -> io::Result<TcpStream> {
    TcpStream::connect(address(response, interface))
}
