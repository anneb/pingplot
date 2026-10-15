# PingPlot

PingPlotter-style traceroute/ping monitor for Windows. Single exe, no service, no admin rights
(uses the Windows ICMP API `IcmpSendEcho`, like WinMTR).

Features: per-hop loss and latency stats, latency graph across hops, timeline for the selected hop,
adjustable interval and focus period, copy results as text.

## Usage

```
pingplot [host]
```

## Build (from Linux/WSL, no Microsoft toolchain)

Requires Rust (rustup, target `x86_64-pc-windows-gnu`), Zig (`pip install ziglang`) and `cargo-zigbuild`.

```
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/pingplot.exe`.
Console engine test: `cargo zigbuild --release --target x86_64-pc-windows-gnu --example trace`.

## License

MIT, see [LICENSE](LICENSE).
