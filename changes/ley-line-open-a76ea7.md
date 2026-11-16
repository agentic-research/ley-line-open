Section: Security
- **quinn-udp 0.5.16 (RUSTSEC-2026-0538).** quinn-udp 0.5.15's receive loop could spin after a truncated datagram. It reached `leyline-runtime` through nono, sigstore, reqwest and quinn. The lock moves to the semver-compatible 0.5.16 (`ley-line-open-a76ea7`).
