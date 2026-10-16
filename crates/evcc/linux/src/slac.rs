//! SLAC over a raw packet socket (Linux): drives [`evcc_core::slac::Machine`] on the charging
//! interface. Needs `CAP_NET_RAW`.
//!
//! The powerline modem is behind the [`Modem`] trait: the machine says when to configure it
//! with the charger's network key, and the adapter asks it whether the link is up. Without a
//! modem (a virtual Ethernet pair in the tests) [`NoModem`] does nothing and reports the link
//! as up.

use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use evcc_core::slac::{Config, Event, Machine, Outcome};
use evcc_core::Timer;
use homeplug::{ETHERTYPE, Mac, RunId};

use crate::Deadlines;

/// A raw socket bound to one interface that receives HomePlug frames only.
pub struct PacketSocket {
    fd: OwnedFd,
    mac: Mac,
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

impl PacketSocket {
    pub fn open(interface: &str) -> io::Result<Self> {
        let ifindex = crate::sdp::interface_index(interface)?;
        let protocol = ETHERTYPE.to_be();
        // SAFETY: plain system calls with valid arguments; the descriptor is owned right away.
        let fd = unsafe {
            let fd = check(libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, i32::from(protocol)))?;
            OwnedFd::from_raw_fd(fd)
        };
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = protocol;
        addr.sll_ifindex = ifindex as i32;
        // SAFETY: `addr` is a valid sockaddr_ll of the stated length.
        check(unsafe {
            libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), std::mem::size_of::<libc::sockaddr_ll>() as u32)
        })?;
        // The kernel reports the interface's hardware address for a bound packet socket (sysfs
        // would show the process's network namespace, not the thread's).
        let mut bound: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_ll>() as u32;
        // SAFETY: `bound` is a valid sockaddr_ll of the stated length.
        check(unsafe { libc::getsockname(fd.as_raw_fd(), (&raw mut bound).cast(), &mut len) })?;
        if bound.sll_halen != 6 {
            return Err(io::Error::new(ErrorKind::InvalidData, "interface has no Ethernet address"));
        }
        let mac: Mac = bound.sll_addr[..6].try_into().unwrap();
        Ok(Self { fd, mac })
    }

    /// The interface's hardware address.
    pub fn mac(&self) -> Mac {
        self.mac
    }

    pub fn send(&self, frame: &[u8]) -> io::Result<()> {
        // SAFETY: the pointer and length describe `frame`.
        let n = check(unsafe { libc::send(self.fd.as_raw_fd(), frame.as_ptr().cast(), frame.len(), 0) } as i32)?;
        if n as usize == frame.len() { Ok(()) } else { Err(ErrorKind::WriteZero.into()) }
    }

    /// Waits up to `timeout` (forever for `None`) for a frame from another station; the
    /// socket also sees the frames it sent itself, which are skipped. `Ok(None)` is a timeout.
    pub fn recv(&self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<Option<usize>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            self.set_timeout(left)?;
            let mut from: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            let mut from_len = std::mem::size_of::<libc::sockaddr_ll>() as u32;
            // SAFETY: the buffers are valid for the lengths given.
            let n = unsafe {
                libc::recvfrom(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0, (&raw mut from).cast(), &mut from_len)
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                    return Ok(None);
                }
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if from.sll_pkttype == libc::PACKET_OUTGOING {
                continue;
            }
            return Ok(Some(n as usize));
        }
    }

    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        // A zero timeval would mean "block forever".
        let tv = match timeout {
            Some(t) => {
                let t = t.max(Duration::from_millis(1));
                libc::timeval { tv_sec: t.as_secs() as libc::time_t, tv_usec: t.subsec_micros() as libc::suseconds_t }
            }
            None => libc::timeval { tv_sec: 0, tv_usec: 0 },
        };
        // SAFETY: `tv` is a valid timeval of the stated length.
        check(unsafe {
            libc::setsockopt(self.fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw const tv).cast(), std::mem::size_of::<libc::timeval>() as u32)
        })?;
        Ok(())
    }
}

/// The powerline modem of the EV.
pub trait Modem {
    /// Configures the modem with the charger's network (CM_SET_KEY.REQ).
    fn set_key(&mut self, nid: [u8; 7], nmk: [u8; 16]) -> io::Result<()>;
    /// Whether the link to the charger is up. Polled after the key is set.
    fn link_up(&mut self) -> io::Result<bool>;
}

/// No modem: the interface is the link already.
pub struct NoModem;

impl Modem for NoModem {
    fn set_key(&mut self, _nid: [u8; 7], _nmk: [u8; 16]) -> io::Result<()> {
        Ok(())
    }

    fn link_up(&mut self) -> io::Result<bool> {
        Ok(true)
    }
}

/// How often the modem is asked whether the link is up.
const LINK_POLL: Duration = Duration::from_millis(50);

/// Runs SLAC on `socket` until the machine has an outcome.
pub fn run(socket: &PacketSocket, config: Config, run_id: RunId, modem: &mut impl Modem) -> io::Result<Outcome> {
    enum Next {
        Frame(usize),
        Timer(Timer),
        Link,
    }
    let mut machine = Machine::new(config);
    let mut deadlines = Deadlines::default();
    let (mut tx, mut rx) = ([0u8; 128], [0u8; 2048]);
    let mut joining = false;
    let mut next = None::<Next>;
    let mut started = false;
    loop {
        let step = match (started, &next) {
            (false, _) => {
                started = true;
                machine.handle(Event::Start { mac: socket.mac(), run_id }, &mut tx)
            }
            (_, Some(Next::Frame(n))) => machine.handle(Event::Frame(&rx[..*n]), &mut tx),
            (_, Some(Next::Timer(t))) => machine.handle(Event::TimerExpired(*t), &mut tx),
            (_, Some(Next::Link)) => machine.handle(Event::LinkUp, &mut tx),
            (_, None) => unreachable!("every iteration after the first has an event"),
        };
        if let Some(n) = step.send {
            socket.send(&tx[..n])?;
        }
        for cmd in step.timers.iter() {
            deadlines.apply(cmd);
        }
        if let Some((nid, nmk)) = step.set_key {
            modem.set_key(nid, nmk)?;
            joining = true;
        }
        if let Some(outcome) = step.outcome {
            return Ok(outcome);
        }
        // Wait for the next frame, timer expiry or link change.
        next = loop {
            if joining && modem.link_up()? {
                joining = false;
                break Some(Next::Link);
            }
            let pending = deadlines.next();
            let mut wait = match pending {
                Some((_, d)) => Some(d.saturating_duration_since(Instant::now())),
                None => None,
            };
            if joining {
                wait = Some(wait.map_or(LINK_POLL, |w| w.min(LINK_POLL)));
            }
            if let (Some((timer, _)), Some(Duration::ZERO)) = (pending, wait) {
                deadlines.clear(timer);
                break Some(Next::Timer(timer));
            }
            if let Some(n) = socket.recv(&mut rx, wait)? {
                break Some(Next::Frame(n));
            }
            if let Some((timer, d)) = pending {
                if d <= Instant::now() {
                    deadlines.clear(timer);
                    break Some(Next::Timer(timer));
                }
            }
        };
    }
}
