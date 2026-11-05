Section: Security

- **rustls 0.23.46** (bead `ley-line-open-642105`). The binary reaches rustls
  through sigstore verification (`nono` -> `sigstore-rekor` -> `reqwest`).
  0.23.45 carried four advisories, fixed in 0.23.46:
  - `Debug` formatting of a session exposed TLS master secrets and resumption
    PSKs;
  - Brotli certificate decompression could allocate up to 1 GiB before
    authentication;
  - an unauthenticated ServerHello could downgrade later key exchanges to
    classical X25519 only;
  - an ECH rejection could expose buffered application data.
