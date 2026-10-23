//! SLAC over a raw packet socket (Linux): drives [`evcc_core::slac::Machine`] on the charging
//! interface. Needs `CAP_NET_RAW`.
//!
//! The powerline modem is behind the [`Modem`] trait: the machine says when to configure it
//! with the charger's network key, and the adapter asks it whether the link is up. Without a
//! modem (a virtual Ethernet pair in the tests) [`NoModem`] does nothing and reports the link
//! as up.

use std::cell::Cell;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use evcc_core::slac::{Config, Event, Machine, Outcome, Toggle};
use evcc_core::Timer;
use homeplug::{ETHERTYPE, Mac, RunId};

use crate::Deadlines;

/// A raw socket bound to one interface that receives HomePlug frames only.
pub struct PacketSocket {
    fd: OwnedFd,
    mac: Mac,
    /// The socket sees every protocol (see [`PacketSocket::open_any`]): other frames are skipped.
    filter: bool,
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

impl PacketSocket {
    pub fn open(interface: &str) -> io::Result<Self> {
        Self::open_with(interface, false)
    }

    /// Like [`PacketSocket::open`] for an interface that is a port of a bridge (a board whose
    /// powerline modem is bridged to its Ethernet port): the kernel hands a frame addressed to
    /// the host to the bridge, and a socket bound to the HomePlug protocol never sees it. This
    /// one is bound to all protocols and skips the others.
    pub fn open_any(interface: &str) -> io::Result<Self> {
        Self::open_with(interface, true)
    }

    fn open_with(interface: &str, any: bool) -> io::Result<Self> {
        let ifindex = crate::sdp::interface_index(interface)?;
        let protocol = if any { (libc::ETH_P_ALL as u16).to_be() } else { ETHERTYPE.to_be() };
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
        Ok(Self { fd, mac, filter: any })
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
            if self.filter && (n < 14 || buf[12..14] != ETHERTYPE.to_be_bytes()) {
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
    /// Whether the link, once up, has been gone for longer than a short debounce time (a modem drops its link for a moment
    /// when the powerline is noisy). Polled by the host while the session runs; a modem that has to be answered between
    /// the requests is served by this call as well.
    fn link_lost(&mut self) -> io::Result<bool> {
        Ok(false)
    }
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

/// The EV's control pilot, for the validation by BCB-Toggle (ISO 15118-3 9.4).
pub trait ControlPilot {
    /// Makes the sequence of [`Toggle`]: from state B, `count` times state C and back to B,
    /// every state `state_ms` long. Returns when the pilot is in state B again. Blocks, which is
    /// fine: nothing else happens in SLAC while the charger counts.
    fn toggle(&mut self, toggle: Toggle) -> io::Result<()>;
}

/// No access to the control pilot (a bench with a veth cable): validation cannot succeed.
pub struct NoPilot;

impl ControlPilot for NoPilot {
    fn toggle(&mut self, _toggle: Toggle) -> io::Result<()> {
        Ok(())
    }
}

/// How often the modem is asked whether the link is up.
const LINK_POLL: Duration = Duration::from_millis(50);

/// Runs SLAC on `socket` until the machine has an outcome.
pub fn run(socket: &PacketSocket, config: Config, run_id: RunId, modem: &mut impl Modem) -> io::Result<Outcome> {
    run_with_pilot(socket, config, run_id, modem, &mut NoPilot)
}

/// [`run`] for a vehicle that can toggle its control pilot, so that [`Config::validation`] works.
pub fn run_with_pilot(
    socket: &PacketSocket,
    config: Config,
    run_id: RunId,
    modem: &mut impl Modem,
    pilot: &mut impl ControlPilot,
) -> io::Result<Outcome> {
    enum Next {
        Frame(usize),
        Timer(Timer),
        Link,
    }
    let mut machine = Machine::new(config);
    let mut deadlines = Deadlines::default();
    let (mut tx, mut rx) = ([0u8; 256], [0u8; 2048]);
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
        if let Some(toggle) = step.toggle {
            pilot.toggle(toggle)?;
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
            let mut wait = pending.map(|(_, d)| d.saturating_duration_since(Instant::now()));
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
            if let Some((timer, d)) = pending
                && d <= Instant::now() {
                    deadlines.clear(timer);
                    break Some(Next::Timer(timer));
                }
        };
    }
}


/// Which message gives a QCA modem its network key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyMessage {
    /// The standard CM_SET_KEY.REQ (HomePlug AV): the host works out the network id. Checked on a QCA7005.
    #[default]
    Standard,
    /// The vendor VS_SET_KEY.REQ (Qualcomm Atheros): the modem works the network id out itself. The layout is the one of the
    /// Qualcomm Atheros QCA7000 documentation as other stacks use it; it was not checked on a modem here.
    Vendor,
}

/// How the link to the charger is read from a QCA modem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkCheck {
    /// VS_PL_LNK_STATUS: the modem says whether it has a link.
    Status,
    /// VS_NW_INFO: the modem has joined the network of its key and someone else coordinates it.
    Info,
}

/// A Qualcomm QCA7000 family modem (QCA7000, QCA7005, QCA7020) behind a network interface of the
/// host, such as the `qcaspi` driver's: the modem takes its network key from a standard
/// CM_SET_KEY.REQ sent to the local device address (or the vendor VS_SET_KEY.REQ, see
/// [`KeyMessage`]), and reports its software version to a vendor message ([`homeplug::modem`]).
///
/// The modem is known to misbehave when it is given keys in quick succession (other stacks leave 4 s between two
/// key requests), so a key request waits until [`QcaModem::KEY_SPACING`] has passed since the last one.
///
/// A modem that reports its link by indication (VS_HST_ACTION.IND) is answered whenever the driver reads from it, and
/// [`Modem::link_lost`] counts an indication of a lost link, like a link status without a link, as lost once it lasted for
/// [`QcaModem::LINK_LOSS_DEBOUNCE`] (the modem drops its link for a moment when the powerline is noisy).
///
/// The link check asks the modem for its link status (VS_PL_LNK_STATUS) and, if it does not know that
/// message, which network it joined (VS_NW_INFO: up when it has joined the network of the key it was
/// given and someone else coordinates it). A modem that answers neither gets the fallback of a settle
/// time after the key (the modem needs a moment to form the network with the charger's; the discovery
/// that follows repeats its requests, so a link that is up a little later only costs a repeat).
pub struct QcaModem {
    socket: PacketSocket,
    settle: Duration,
    reply_timeout: Duration,
    key_message: KeyMessage,
    key_spacing: Duration,
    /// When the last key request went out.
    last_key: Option<Instant>,
    /// The way the modem answered a link check before.
    link_check: Option<LinkCheck>,
    /// When the modem was given its key, and the network id of that key.
    keyed: Option<(Instant, [u8; 7])>,
    link_loss_debounce: Duration,
    link_poll: Duration,
    /// The link was up ([`Modem::link_up`]) and is watched.
    watching: bool,
    /// Since when the modem reports no link (by indication or link status) while it is watched.
    down_since: Cell<Option<Instant>>,
    /// When the link status was last asked for while watching.
    last_poll: Option<Instant>,
}

impl QcaModem {
    /// The default settle time after the key.
    pub const SETTLE: Duration = Duration::from_millis(1500);
    /// The shortest time between two key requests.
    pub const KEY_SPACING: Duration = Duration::from_secs(4);
    /// The default time the link may be gone before [`Modem::link_lost`] says so.
    pub const LINK_LOSS_DEBOUNCE: Duration = Duration::from_millis(500);
    /// How often the watched link is asked for between the modem's indications.
    pub const LINK_POLL: Duration = Duration::from_secs(1);

    pub fn open(interface: &str) -> io::Result<Self> {
        Ok(Self::new(PacketSocket::open_any(interface)?))
    }

    pub fn new(socket: PacketSocket) -> Self {
        Self {
            socket,
            settle: Self::SETTLE,
            reply_timeout: Duration::from_millis(1000),
            key_message: KeyMessage::default(),
            key_spacing: Self::KEY_SPACING,
            last_key: None,
            link_check: None,
            keyed: None,
            link_loss_debounce: Self::LINK_LOSS_DEBOUNCE,
            link_poll: Self::LINK_POLL,
            watching: false,
            down_since: Cell::new(None),
            last_poll: None,
        }
    }

    /// How long to wait for the modem's answer to a request.
    pub fn with_reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// The message that sets the key.
    pub fn with_key_message(mut self, message: KeyMessage) -> Self {
        self.key_message = message;
        self
    }

    /// The shortest time between two key requests (the default is [`QcaModem::KEY_SPACING`]).
    pub fn with_key_spacing(mut self, spacing: Duration) -> Self {
        self.key_spacing = spacing;
        self
    }

    /// How long the link may be gone before [`Modem::link_lost`] says so (the default is [`QcaModem::LINK_LOSS_DEBOUNCE`]).
    pub fn with_link_loss_debounce(mut self, debounce: Duration) -> Self {
        self.link_loss_debounce = debounce;
        self
    }

    /// How often the watched link is asked for between the modem's indications (the default is [`QcaModem::LINK_POLL`]).
    pub fn with_link_poll(mut self, interval: Duration) -> Self {
        self.link_poll = interval;
        self
    }

    /// The software version of the modem, such as `MAC-QCA7005-3.3.0.31-00-20231004-CS`; an error
    /// when no modem answers.
    pub fn version(&self) -> io::Result<String> {
        let mut out = [0u8; homeplug::modem::FRAME_MAX];
        let n = homeplug::modem::sw_ver_req(self.socket.mac(), &mut out).ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        self.socket.send(&out[..n])?;
        let frame = self.wait_for(|f| homeplug::modem::parse_sw_ver_cnf(f).map(|v| (v.status, v.version.to_string())))?;
        match frame {
            (0, version) => Ok(version),
            (status, _) => Err(io::Error::other(format!("the modem answered the version request with status {status}"))),
        }
    }

    /// The networks the modem belongs to.
    pub fn network_info(&self) -> io::Result<homeplug::modem::NwInfo> {
        let mut out = [0u8; homeplug::modem::FRAME_MAX];
        let n = homeplug::modem::nw_info_req(self.socket.mac(), &mut out).ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        self.socket.send(&out[..n])?;
        self.wait_for(homeplug::modem::parse_nw_info_cnf)
    }

    /// Whether the modem has a link to another station (VS_PL_LNK_STATUS).
    pub fn link_status(&self) -> io::Result<homeplug::modem::LinkStatus> {
        let mut out = [0u8; homeplug::modem::FRAME_MAX];
        let n = homeplug::modem::lnk_status_req(self.socket.mac(), &mut out).ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        self.socket.send(&out[..n])?;
        self.wait_for(homeplug::modem::parse_lnk_status_cnf)
    }

    /// Frames until `parse` accepts one or the reply timeout passes.
    fn wait_for<T>(&self, parse: impl Fn(&[u8]) -> Option<T>) -> io::Result<T> {
        let deadline = Instant::now() + self.reply_timeout;
        let mut buf = [0u8; 2048];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(ErrorKind::TimedOut, "the modem did not answer"));
            }
            if let Some(n) = self.socket.recv(&mut buf, Some(left))? {
                if self.answer_indication(&buf[..n])? {
                    continue;
                }
                if let Some(found) = parse(&buf[..n]) {
                    return Ok(found);
                }
            }
        }
    }

    /// Acknowledges a VS_HST_ACTION.IND (the modem repeats it until it is) and notes what it says about the link. False for
    /// any other frame.
    fn answer_indication(&self, frame: &[u8]) -> io::Result<bool> {
        let Some(ind) = homeplug::modem::parse_hst_action_ind(frame) else { return Ok(false) };
        match ind.action {
            homeplug::modem::HAR_LINK_ONLINE => self.down_since.set(None),
            homeplug::modem::HAR_LINK_OFFLINE if self.down_since.get().is_none() => self.down_since.set(Some(Instant::now())),
            _ => {}
        }
        let mut out = [0u8; homeplug::modem::FRAME_MAX];
        let n = homeplug::modem::hst_action_rsp(self.socket.mac(), &ind, true, &mut out).ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        self.socket.send(&out[..n])?;
        Ok(true)
    }
}

impl Modem for QcaModem {
    fn set_key(&mut self, nid: [u8; 7], nmk: [u8; 16]) -> io::Result<()> {
        if let Some(last) = self.last_key {
            let wait = self.key_spacing.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        let mut out = [0u8; homeplug::modem::FRAME_MAX];
        let n = match self.key_message {
            KeyMessage::Standard => homeplug::modem::set_key_req(self.socket.mac(), nid, nmk, &mut out),
            KeyMessage::Vendor => homeplug::modem::vs_set_key_req(self.socket.mac(), nmk, &mut out),
        }
        .ok_or_else(|| io::Error::from(ErrorKind::InvalidInput))?;
        self.last_key = Some(Instant::now());
        self.socket.send(&out[..n])?;
        let refused = match self.key_message {
            KeyMessage::Standard => {
                let result = self.wait_for(homeplug::modem::parse_set_key_cnf)?;
                (result != homeplug::modem::SET_KEY_DONE).then(|| format!("result {result}"))
            }
            KeyMessage::Vendor => {
                let status = self.wait_for(homeplug::modem::parse_vs_set_key_cnf)?;
                (status != 0).then(|| format!("status {status}"))
            }
        };
        if let Some(why) = refused {
            return Err(io::Error::other(format!("the modem refused the key ({why})")));
        }
        self.keyed = Some((Instant::now(), nid));
        self.link_check = None;
        self.watching = false;
        self.down_since.set(None);
        Ok(())
    }

    fn link_up(&mut self) -> io::Result<bool> {
        let Some((at, nid)) = self.keyed else { return Ok(false) };
        // The way that worked before first; otherwise the link status, then the network information.
        let order = match self.link_check {
            Some(LinkCheck::Info) => [LinkCheck::Info, LinkCheck::Status],
            _ => [LinkCheck::Status, LinkCheck::Info],
        };
        for check in order {
            let answer = match check {
                LinkCheck::Status => self.link_status().map(|s| s.status == 0 && s.link),
                LinkCheck::Info => self.network_info().map(|info| info.joined(nid)),
            };
            match answer {
                Ok(up) => {
                    self.link_check = Some(check);
                    if up {
                        self.watching = true;
                        self.last_poll = Some(Instant::now());
                        self.down_since.set(None);
                    }
                    return Ok(up);
                }
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
            // A way that answered once is not tried again after it stayed silent only to be told otherwise.
            if self.link_check == Some(check) {
                self.link_check = None;
            }
        }
        // A modem that knows neither message: the settle time has to do.
        Ok(at.elapsed() >= self.settle)
    }

    fn link_lost(&mut self) -> io::Result<bool> {
        // The modem's indications first (they are answered as they come), then the link status once in a while.
        let mut buf = [0u8; 2048];
        while let Some(n) = self.socket.recv(&mut buf, Some(Duration::from_millis(1)))? {
            self.answer_indication(&buf[..n])?;
        }
        if !self.watching {
            return Ok(false);
        }
        if self.last_poll.is_none_or(|at| at.elapsed() >= self.link_poll) {
            self.last_poll = Some(Instant::now());
            match self.link_status() {
                Ok(s) if s.status == 0 && s.link => self.down_since.set(None),
                Ok(_) if self.down_since.get().is_none() => self.down_since.set(Some(Instant::now())),
                Ok(_) => {}
                // A modem that does not answer is not told to have lost the link: the session has its own timeouts.
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
        Ok(self.down_since.get().is_some_and(|since| since.elapsed() >= self.link_loss_debounce))
    }
}
