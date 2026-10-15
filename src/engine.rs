//! Trace engine: one thread per TTL, each pinging the target with that TTL
//! at a fixed interval (same approach as WinMTR). Needs no administrator/root
//! rights: Windows ICMP API (IcmpSendEcho) or ICMP datagram sockets on Unix.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_HOPS: usize = 30;
const MAX_SAMPLES: usize = 50_000;
const TIMEOUT_MS: u32 = 2000;

#[derive(Clone, Copy)]
pub struct Sample {
    /// Seconds since trace start (send time).
    pub t: f64,
    /// Round trip time in ms, None = lost.
    pub rtt: Option<f64>,
    /// Measured in direct mode (reply to the TTL probe or the direct ping).
    pub direct: bool,
}

#[derive(Default, Clone)]
pub struct Hop {
    pub addr: Option<Ipv4Addr>,
    pub name: Option<String>,
    pub samples: VecDeque<Sample>,
}

#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub sent: usize,
    pub recv: usize,
    pub best: f64,
    pub avg: f64,
    pub worst: f64,
    pub last: Option<f64>,
    pub loss: f64,
}

impl Hop {
    /// Statistics over samples sent at or after `since` (seconds since start).
    pub fn stats(&self, since: f64) -> Stats {
        let mut s = Stats { best: f64::MAX, ..Default::default() };
        let mut sum = 0.0;
        for smp in self.samples.iter().rev().take_while(|x| x.t >= since) {
            s.sent += 1;
            if let Some(r) = smp.rtt {
                s.recv += 1;
                sum += r;
                s.best = s.best.min(r);
                s.worst = s.worst.max(r);
            }
        }
        s.last = self.samples.back().and_then(|x| x.rtt);
        if s.recv > 0 {
            s.avg = sum / s.recv as f64;
        } else {
            s.best = 0.0;
        }
        if s.sent > 0 {
            s.loss = 100.0 * (s.sent - s.recv) as f64 / s.sent as f64;
        }
        s
    }

    pub fn label(&self) -> String {
        match (&self.addr, &self.name) {
            (Some(a), Some(n)) if n != &a.to_string() => format!("{n} [{a}]"),
            (Some(a), _) => a.to_string(),
            (None, _) => "* no reply *".into(),
        }
    }
}

pub struct Trace {
    pub host: String,
    pub target: Ipv4Addr,
    pub hops: Arc<Mutex<Vec<Hop>>>,
    pub interval_ms: Arc<AtomicU64>,
    /// Measure hops by pinging their address directly (like pathping) instead
    /// of using the TTL probe. Helps with routers that rarely send Time Exceeded.
    pub direct: Arc<AtomicBool>,
    dest_ttl: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    start: Instant,
}

pub fn resolve(host: &str) -> Result<Ipv4Addr, String> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    (host, 0)
        .to_socket_addrs()
        .map_err(|e| format!("Cannot resolve {host}: {e}"))?
        .find_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .ok_or_else(|| format!("No IPv4 address for {host}"))
}

impl Trace {
    pub fn start(host: &str, interval_ms: u64) -> Result<Self, String> {
        let target = resolve(host)?;
        let t = Trace {
            host: host.to_string(),
            target,
            hops: Arc::new(Mutex::new(vec![Hop::default(); MAX_HOPS])),
            interval_ms: Arc::new(AtomicU64::new(interval_ms)),
            direct: Arc::new(AtomicBool::new(false)),
            dest_ttl: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            start: Instant::now(),
        };
        for ttl in 1..=MAX_HOPS {
            let ctx = HopCtx {
                ttl,
                target,
                hops: t.hops.clone(),
                interval_ms: t.interval_ms.clone(),
                direct: t.direct.clone(),
                dest_ttl: t.dest_ttl.clone(),
                stop: t.stop.clone(),
                start: t.start,
            };
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(ttl as u64 * 30));
                hop_thread(ctx)
            });
        }
        Ok(t)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Number of hops to display: up to the destination, or the last hop that answered.
    pub fn hop_count(&self, hops: &[Hop]) -> usize {
        match self.dest_ttl.load(Ordering::Relaxed) {
            0 => hops.iter().rposition(|h| h.addr.is_some()).map_or(0, |i| i + 1),
            d => d,
        }
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone)]
struct HopCtx {
    ttl: usize,
    target: Ipv4Addr,
    hops: Arc<Mutex<Vec<Hop>>>,
    interval_ms: Arc<AtomicU64>,
    direct: Arc<AtomicBool>,
    dest_ttl: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    start: Instant,
}

impl HopCtx {
    fn stopped(&self) -> bool {
        let d = self.dest_ttl.load(Ordering::Relaxed);
        self.stop.load(Ordering::Relaxed) || (d != 0 && self.ttl > d)
    }

    /// Address to ping directly, if direct mode applies to this hop.
    fn direct_addr(&self, hop: &Hop) -> Option<Ipv4Addr> {
        hop.addr.filter(|&a| a != self.target && self.direct.load(Ordering::Relaxed))
    }

    fn record(&self, hop: &mut Hop, t: f64, rtt: Option<f64>, direct: bool) {
        // Don't mix both kinds of measurement in one history.
        if hop.samples.back().is_some_and(|s| s.direct != direct) {
            hop.samples.clear();
        }
        hop.samples.push_back(Sample { t, rtt, direct });
        if hop.samples.len() > MAX_SAMPLES {
            hop.samples.pop_front();
        }
    }

    /// Wait until one interval after `sent_at`, in small steps so Stop reacts quickly.
    fn wait(&self, sent_at: Instant) {
        let until = sent_at + Duration::from_millis(self.interval_ms.load(Ordering::Relaxed));
        while Instant::now() < until && !self.stop.load(Ordering::Relaxed) {
            thread::sleep((until - Instant::now()).min(Duration::from_millis(100)));
        }
    }
}

/// TTL-limited probe each interval to discover the hop. In direct mode a
/// normal echo to the hop's address is sent at the same time, and the hop
/// counts as answering if either probe gets a reply.
fn hop_thread(cx: HopCtx) {
    let ttl = cx.ttl;
    let Some(pinger) = icmp::Pinger::new() else { return };
    let mut direct_pinger = icmp::Pinger::new();
    let ms_since = |at: Instant| at.elapsed().as_secs_f64() * 1000.0;
    while !cx.stopped() {
        let sent_at = Instant::now();
        let t = (sent_at - cx.start).as_secs_f64();
        let direct_to = cx.direct_addr(&cx.hops.lock().unwrap()[ttl - 1]);
        let probe = || {
            let reply = pinger.ping(cx.target, ttl as u8, TIMEOUT_MS);
            (reply, reply.map(|_| ms_since(sent_at)))
        };
        let ((reply, rtt), direct_rtt) = match (direct_to, direct_pinger.take()) {
            (Some(addr), Some(dp)) => thread::scope(|s| {
                let h = s.spawn(move || {
                    let rtt = (dp.ping(addr, 64, TIMEOUT_MS) == Some(addr)).then(|| ms_since(sent_at));
                    (dp, rtt)
                });
                let ttl_result = probe();
                let (dp, rtt) = h.join().unwrap();
                direct_pinger = Some(dp);
                (ttl_result, rtt)
            }),
            (_, dp) => {
                direct_pinger = dp;
                (probe(), None)
            }
        };

        if reply == Some(cx.target) {
            // Lowest TTL that reaches the target is the destination.
            let _ = cx.dest_ttl.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                (cur == 0 || ttl < cur).then_some(ttl)
            });
        }
        {
            let mut hs = cx.hops.lock().unwrap();
            let hop = &mut hs[ttl - 1];
            if let Some(addr) = reply {
                if hop.addr != Some(addr) {
                    hop.addr = Some(addr);
                    hop.name = None;
                    let hops = cx.hops.clone();
                    thread::spawn(move || {
                        if let Ok(name) = dns_lookup::lookup_addr(&IpAddr::V4(addr)) {
                            let mut hs = hops.lock().unwrap();
                            if hs[ttl - 1].addr == Some(addr) {
                                hs[ttl - 1].name = Some(name);
                            }
                        }
                    });
                }
            }
            if direct_to.is_some() {
                // Prefer the direct RTT: it is not delayed by Time Exceeded generation.
                cx.record(hop, t, direct_rtt.or(rtt), true);
            } else {
                cx.record(hop, t, rtt, false);
            }
        }
        cx.wait(sent_at);
    }
}

#[cfg(windows)]
mod icmp {
    use std::ffi::c_void;
    use std::net::Ipv4Addr;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY, IP_OPTION_INFORMATION,
    };

    const IP_SUCCESS: u32 = 0;
    const IP_TTL_EXPIRED_TRANSIT: u32 = 11013;

    pub struct Pinger(HANDLE);
    // The ICMP handle may be used from any thread (moved to a helper thread in direct mode).
    unsafe impl Send for Pinger {}

    impl Pinger {
        pub fn new() -> Option<Self> {
            let h = unsafe { IcmpCreateFile() };
            (h != INVALID_HANDLE_VALUE && !h.is_null()).then_some(Pinger(h))
        }

        /// Returns the address of the replying host (router or target).
        pub fn ping(&self, dst: Ipv4Addr, ttl: u8, timeout_ms: u32) -> Option<Ipv4Addr> {
            let data = [0x41u8; 32];
            let mut opts: IP_OPTION_INFORMATION = unsafe { std::mem::zeroed() };
            opts.Ttl = ttl;
            let mut buf = [0u64; 256];
            let n = unsafe {
                IcmpSendEcho(
                    self.0,
                    u32::from_ne_bytes(dst.octets()),
                    data.as_ptr() as *const c_void,
                    data.len() as u16,
                    &opts,
                    buf.as_mut_ptr() as *mut c_void,
                    std::mem::size_of_val(&buf) as u32,
                    timeout_ms,
                )
            };
            if n == 0 {
                return None;
            }
            let reply = unsafe { &*(buf.as_ptr() as *const ICMP_ECHO_REPLY) };
            match reply.Status {
                IP_SUCCESS | IP_TTL_EXPIRED_TRANSIT => Some(Ipv4Addr::from(reply.Address.to_ne_bytes())),
                _ => None,
            }
        }
    }

    impl Drop for Pinger {
        fn drop(&mut self) {
            unsafe { IcmpCloseHandle(self.0) };
        }
    }
}

/// Linux and macOS: unprivileged ICMP datagram socket (SOCK_DGRAM, IPPROTO_ICMP).
/// On Linux this needs the user's group in net.ipv4.ping_group_range (default on
/// most distributions and WSL). Linux reports Time Exceeded through the socket
/// error queue (IP_RECVERR); macOS delivers it as a normal packet with IP header.
#[cfg(unix)]
mod icmp {
    use std::cell::Cell;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::time::{Duration, Instant};

    const ECHO_REPLY: u8 = 0;
    #[cfg(not(target_os = "linux"))]
    const DEST_UNREACH: u8 = 3;
    const ECHO_REQUEST: u8 = 8;
    const TIME_EXCEEDED: u8 = 11;

    static NEXT_ID: AtomicU16 = AtomicU16::new(0);

    enum Got {
        Reply(Ipv4Addr),
        Failed,
        Other,
    }

    pub struct Pinger {
        fd: libc::c_int,
        /// Echo identifier; Linux replaces it with the socket's own id.
        id: u16,
        seq: Cell<u16>,
    }

    impl Pinger {
        pub fn new() -> Option<Self> {
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
            if fd < 0 {
                return None;
            }
            let p = Pinger {
                fd,
                id: (std::process::id() as u16).wrapping_add(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
                seq: Cell::new(0),
            };
            #[cfg(target_os = "linux")]
            if !p.set_opt(libc::IP_RECVERR, 1) {
                return None;
            }
            Some(p)
        }

        fn set_opt(&self, opt: libc::c_int, val: libc::c_int) -> bool {
            unsafe {
                libc::setsockopt(
                    self.fd,
                    libc::IPPROTO_IP,
                    opt,
                    &val as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&val) as libc::socklen_t,
                ) == 0
            }
        }

        /// Returns the address of the replying host (router or target).
        pub fn ping(&self, dst: Ipv4Addr, ttl: u8, timeout_ms: u32) -> Option<Ipv4Addr> {
            let seq = self.seq.get().wrapping_add(1);
            self.seq.set(seq);
            if !self.set_opt(libc::IP_TTL, ttl as libc::c_int) || !self.send(dst, seq) {
                return None;
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                let mut pfd = libc::pollfd { fd: self.fd, events: libc::POLLIN, revents: 0 };
                if unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) } <= 0 {
                    continue;
                }
                while let Some(got) = self.read(dst, seq) {
                    match got {
                        Got::Reply(addr) => return Some(addr),
                        Got::Failed => return None,
                        Got::Other => {}
                    }
                }
            }
        }

        fn send(&self, dst: Ipv4Addr, seq: u16) -> bool {
            let mut pkt = [0x41u8; 40];
            pkt[0] = ECHO_REQUEST;
            pkt[1] = 0;
            pkt[2..4].fill(0);
            pkt[4..6].copy_from_slice(&self.id.to_be_bytes());
            pkt[6..8].copy_from_slice(&seq.to_be_bytes());
            let sum = checksum(&pkt);
            pkt[2..4].copy_from_slice(&sum.to_be_bytes());
            let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            #[cfg(target_os = "macos")]
            {
                sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
            }
            sa.sin_family = libc::AF_INET as libc::sa_family_t;
            sa.sin_addr.s_addr = u32::from_ne_bytes(dst.octets());
            let n = unsafe {
                libc::sendto(
                    self.fd,
                    pkt.as_ptr() as *const libc::c_void,
                    pkt.len(),
                    0,
                    &sa as *const _ as *const libc::sockaddr,
                    std::mem::size_of_val(&sa) as libc::socklen_t,
                )
            };
            n == pkt.len() as isize
        }

        /// Reads one pending packet or error; None when nothing is pending.
        #[cfg(target_os = "linux")]
        fn read(&self, dst: Ipv4Addr, seq: u16) -> Option<Got> {
            let mut buf = [0u8; 512];
            // Router answers (Time Exceeded, Unreachable) arrive on the error queue.
            let mut ctrl = [0u64; 64];
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = std::mem::size_of_val(&ctrl) as _;
            let n = unsafe { libc::recvmsg(self.fd, &mut msg, libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT) };
            if n >= 0 {
                // Payload is our original echo request.
                if n < 8 || buf[6..8] != seq.to_be_bytes() {
                    return Some(Got::Other);
                }
                let mut c = unsafe { libc::CMSG_FIRSTHDR(&msg) };
                while !c.is_null() {
                    let h = unsafe { &*c };
                    if h.cmsg_level == libc::IPPROTO_IP && h.cmsg_type == libc::IP_RECVERR {
                        let ee = unsafe { &*(libc::CMSG_DATA(c) as *const libc::sock_extended_err) };
                        if ee.ee_origin != libc::SO_EE_ORIGIN_ICMP {
                            return Some(Got::Other);
                        }
                        if ee.ee_type != TIME_EXCEEDED {
                            return Some(Got::Failed);
                        }
                        // SO_EE_OFFENDER: sockaddr of the router follows the struct.
                        let from = unsafe {
                            std::ptr::read_unaligned(
                                (ee as *const libc::sock_extended_err).add(1) as *const libc::sockaddr_in,
                            )
                        };
                        return Some(Got::Reply(Ipv4Addr::from(from.sin_addr.s_addr.to_ne_bytes())));
                    }
                    c = unsafe { libc::CMSG_NXTHDR(&msg, c) };
                }
                return Some(Got::Other);
            }
            // Echo replies arrive as normal data, without IP header; the kernel
            // only delivers replies matching this socket's identifier.
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) };
            if n < 0 {
                return None;
            }
            Some(if n >= 8 && buf[0] == ECHO_REPLY && buf[6..8] == seq.to_be_bytes() {
                Got::Reply(dst)
            } else {
                Got::Other
            })
        }

        /// Reads one pending packet; None when nothing is pending. macOS delivers
        /// all ICMP packets including the IP header, so filter on id and sequence.
        #[cfg(not(target_os = "linux"))]
        fn read(&self, _dst: Ipv4Addr, seq: u16) -> Option<Got> {
            let mut buf = [0u8; 512];
            let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), libc::MSG_DONTWAIT) };
            if n < 0 {
                return None;
            }
            let pkt = &buf[..n as usize];
            let ours = |icmp: &[u8]| icmp.len() >= 8 && icmp[4..6] == self.id.to_be_bytes() && icmp[6..8] == seq.to_be_bytes();
            let Some((from, icmp)) = split_ip(pkt) else { return Some(Got::Other) };
            Some(match icmp.first() {
                Some(&ECHO_REPLY) if ours(icmp) => Got::Reply(from),
                Some(&t @ (TIME_EXCEEDED | DEST_UNREACH)) if icmp.len() > 8 => match split_ip(&icmp[8..]) {
                    Some((_, inner)) if inner.first() == Some(&ECHO_REQUEST) && ours(inner) => {
                        if t == TIME_EXCEEDED {
                            Got::Reply(from)
                        } else {
                            Got::Failed
                        }
                    }
                    _ => Got::Other,
                },
                _ => Got::Other,
            })
        }
    }

    /// Splits an IPv4 packet into (source address, payload).
    #[cfg(not(target_os = "linux"))]
    fn split_ip(pkt: &[u8]) -> Option<(Ipv4Addr, &[u8])> {
        let hl = (*pkt.first()? as usize & 0x0f) * 4;
        (pkt.len() >= hl && hl >= 20).then(|| (Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]), &pkt[hl..]))
    }

    fn checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = data.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    impl Drop for Pinger {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }
}
