//! Console test: pingplot-trace <host> [seconds] [direct]
use pingplot::engine::Trace;
use std::time::Duration;

fn main() {
    let host = std::env::args().nth(1).unwrap_or_else(|| "8.8.8.8".into());
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let t = Trace::start(&host, 1000).unwrap();
    if std::env::args().nth(3).as_deref() == Some("direct") {
        t.direct.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    std::thread::sleep(Duration::from_secs(secs));
    let hops = t.hops.lock().unwrap();
    println!("{} ({})", t.host, t.target);
    for (i, h) in hops[..t.hop_count(&hops)].iter().enumerate() {
        let s = h.stats(0.0);
        println!(
            "{:2} {:40} loss {:5.1}% sent {:3} best {:6.1} avg {:6.1} worst {:6.1}",
            i + 1, h.label(), s.loss, s.sent, s.best, s.avg, s.worst
        );
    }
}
