//! Trace engine: one thread per TTL, each pinging the target with that TTL
//! at a fixed interval (same approach as WinMTR). Uses the Windows ICMP API
//! (IcmpSendEcho), which works without administrator rights.

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
            dest_ttl: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            start: Instant::now(),
        };
        for ttl in 1..=MAX_HOPS {
            let (hops, iv, dest, stop, start) =
                (t.hops.clone(), t.interval_ms.clone(), t.dest_ttl.clone(), t.stop.clone(), t.start);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(ttl as u64 * 30));
                hop_thread(ttl, target, hops, iv, dest, stop, start)
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

fn hop_thread(
    ttl: usize,
    target: Ipv4Addr,
    hops: Arc<Mutex<Vec<Hop>>>,
    interval_ms: Arc<AtomicU64>,
    dest_ttl: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    start: Instant,
) {
    let Some(pinger) = icmp::Pinger::new() else { return };
    while !stop.load(Ordering::Relaxed) {
        let d = dest_ttl.load(Ordering::Relaxed);
        if d != 0 && ttl > d {
            return;
        }
        let sent_at = Instant::now();
        let t = (sent_at - start).as_secs_f64();
        let reply = pinger.ping(target, ttl as u8, TIMEOUT_MS);
        let rtt = reply.map(|_| sent_at.elapsed().as_secs_f64() * 1000.0);

        if let Some(addr) = reply {
            if addr == target {
                // Lowest TTL that reaches the target is the destination.
                let _ = dest_ttl.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    (cur == 0 || ttl < cur).then_some(ttl)
                });
            }
        }
        {
            let mut hs = hops.lock().unwrap();
            let hop = &mut hs[ttl - 1];
            if let Some(addr) = reply {
                if hop.addr != Some(addr) {
                    hop.addr = Some(addr);
                    hop.name = None;
                    let hops = hops.clone();
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
            hop.samples.push_back(Sample { t, rtt });
            if hop.samples.len() > MAX_SAMPLES {
                hop.samples.pop_front();
            }
        }
        let wait = Duration::from_millis(interval_ms.load(Ordering::Relaxed));
        let spent = sent_at.elapsed();
        if spent < wait {
            // Sleep in small steps so Stop reacts quickly.
            let until = sent_at + wait;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                thread::sleep((until - Instant::now()).min(Duration::from_millis(100)));
            }
        }
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

#[cfg(not(windows))]
mod icmp {
    use std::net::Ipv4Addr;
    pub struct Pinger;
    impl Pinger {
        pub fn new() -> Option<Self> {
            None
        }
        pub fn ping(&self, _: Ipv4Addr, _: u8, _: u32) -> Option<Ipv4Addr> {
            None
        }
    }
}
