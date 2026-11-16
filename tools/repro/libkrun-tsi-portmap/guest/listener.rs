//! Guest workload for the TSI port-map reproduction.
//!
//! This runs *inside* the microVM. It binds the listeners the host probe
//! expects, records whether each bind succeeded in a marker file on the shared
//! root (so the host can tell "the map refused this" from "the guest had no
//! stack"), and serves them for the probe window. It contains no LLO code and
//! no libkrun code: it is an ordinary static Linux binary.
//!
//! Ports (kept in sync with `run.sh` and `src/embedder.c`):
//!   * TCP 8000   — the one mapped guest port (host reaches it at 18000).
//!   * TCP 19999  — an unmapped guest listener (the stream bug).
//!   * UDP 19998  — an unmapped guest datagram bind (the TSIv4 bind bug).
//!   * a UDP socket that sends one datagram to the host sink, then listens for
//!     replies (the v1.19.x `sendto`-opens-a-host-listener datagram bug).
//!
//! argv: listener <marker_path> <mode> <sink_host:port>
//!   mode = "mapped"  bind every listener above, under a configured map.
//!   mode = "empty"   bind only TCP 19999, under an empty map.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

const TCP_MAPPED: u16 = 8000;
const TCP_UNMAPPED: u16 = 19999;
const UDP_UNMAPPED: u16 = 19998;

/// Appends one `key=value` line to the marker and flushes to the shared root.
/// Also echoes to stdout (the guest console) so the host has a second channel
/// if the shared root turns out to be read-only.
fn record(marker: &str, line: &str) {
    println!("MARK {line}");
    let _ = std::io::stdout().flush();
    match OpenOptions::new().create(true).append(true).open(marker) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
            let _ = f.sync_all();
        }
        Err(e) => {
            println!("MARK marker-write-failed:{}", e.raw_os_error().unwrap_or(-1));
            let _ = std::io::stdout().flush();
        }
    }
}

/// Binds a TCP listener on 0.0.0.0:port and, on success, serves "reached\n" to
/// each connection. Returns the bind outcome string for the marker.
fn serve_tcp(port: u16) -> String {
    match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)) {
        Ok(listener) => {
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if let Ok(mut s) = stream {
                        let _ = s.write_all(b"reached\n");
                    }
                }
            });
            "bound".to_owned()
        }
        Err(e) => format!("bind-failed:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

/// Binds a UDP socket on 0.0.0.0:port. On any datagram received, records the
/// source via `tx`. Returns the bind outcome string.
fn serve_udp(port: u16, label: &'static str, tx: Sender<(&'static str, String)>) -> String {
    match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)) {
        Ok(sock) => {
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                loop {
                    match sock.recv_from(&mut buf) {
                        Ok((_, src)) => {
                            let _ = tx.send((label, src.to_string()));
                        }
                        Err(_) => return,
                    }
                }
            });
            "bound".to_owned()
        }
        Err(e) => format!("bind-failed:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let marker = args.get(1).cloned().unwrap_or_else(|| "/probe-marker".into());
    let mode = args.get(2).cloned().unwrap_or_else(|| "mapped".into());
    let sink = args.get(3).cloned().unwrap_or_default();
    let ttl_secs: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(12);

    let (tx, rx) = mpsc::channel::<(&'static str, String)>();

    // A reader thread drains datagram arrivals into the marker as they happen,
    // so the host sees each inbound reachability the moment it occurs.
    {
        let marker = marker.clone();
        thread::spawn(move || {
            while let Ok((label, src)) = rx.recv() {
                record(&marker, &format!("{label}=reached:{src}"));
            }
        });
    }

    record(&marker, &format!("mode={mode}"));

    if mode == "empty" {
        let r = serve_tcp(TCP_UNMAPPED);
        record(&marker, &format!("tcp_unmapped={r}"));
    } else {
        let r = serve_tcp(TCP_MAPPED);
        record(&marker, &format!("tcp_mapped={r}"));
        let r = serve_tcp(TCP_UNMAPPED);
        record(&marker, &format!("tcp_unmapped={r}"));
        let r = serve_udp(UDP_UNMAPPED, "udp_fixed_inbound", tx.clone());
        record(&marker, &format!("udp_unmapped={r}"));

        // The v1.19.x datagram bug: a single sendto makes the host proxy bind a
        // UDP socket that then forwards datagrams from any source. We send one
        // datagram to the host sink (so the harness learns the proxy's host
        // port), then listen on the same socket for an injected reply from a
        // DIFFERENT host source. If that injected datagram reaches us, the host
        // socket was an open listener, not a reply path.
        if !sink.is_empty() {
            match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
                Ok(sock) => {
                    record(&marker, "udp_sendto=bound");
                    let s2 = sock.try_clone().expect("clone udp");
                    let sink2 = sink.clone();
                    thread::spawn(move || {
                        // Resend periodically so the harness has time to learn
                        // the host port and inject. Bounded by the process TTL.
                        for _ in 0..40 {
                            let _ = s2.send_to(b"guest-sendto\n", &sink2);
                            thread::sleep(Duration::from_millis(250));
                        }
                    });
                    let tx2 = tx.clone();
                    thread::spawn(move || {
                        let mut buf = [0u8; 2048];
                        loop {
                            match sock.recv_from(&mut buf) {
                                Ok((_, src)) => {
                                    let _ = tx2.send(("udp_sendto_inbound", src.to_string()));
                                }
                                Err(_) => return,
                            }
                        }
                    });
                }
                Err(e) => record(
                    &marker,
                    &format!("udp_sendto=bind-failed:{}", e.raw_os_error().unwrap_or(-1)),
                ),
            }
        }
    }

    record(&marker, "ready");
    // Hold the listeners open across the host's probe window, then exit so the
    // VM tears down deterministically rather than relying on a host kill.
    thread::sleep(Duration::from_secs(12));
    record(&marker, "done");
}
