//! Measures chunking throughput so the SIMD question is answered by numbers,
//! not vibes. `cargo run -p leyline-cdc --release --example throughput`.
use std::time::Instant;

fn main() {
    let n = 256 * 1024 * 1024;
    let mut s: u64 = 0x243F_6A88_85A3_08D3;
    let data: Vec<u8> = (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 24) as u8
        })
        .collect();

    // Boundary-finding alone (gearhash rolling hash — the part SIMD affects).
    let t = Instant::now();
    let mut h = gearhash::Hasher::default();
    let (mut off, mut nb) = (0usize, 0usize);
    while let Some(b) = h.next_match(&data[off..], 0x0000_5890_5303_0000) {
        off += b;
        nb += 1;
    }
    let dt_scan = t.elapsed();
    println!(
        "  gearhash scan only: {} boundaries in {:?} = {:.0} MiB/s",
        nb,
        dt_scan,
        (n as f64 / (1024.0 * 1024.0)) / dt_scan.as_secs_f64()
    );

    let t = Instant::now();
    let chunks = leyline_cdc::chunk(&data);
    let dt = t.elapsed();
    println!(
        "arch={} {} MiB -> {} chunks in {:?} = {:.0} MiB/s (includes BLAKE3 of every chunk)",
        std::env::consts::ARCH,
        n / (1024 * 1024),
        chunks.len(),
        dt,
        (n as f64 / (1024.0 * 1024.0)) / dt.as_secs_f64()
    );
}
