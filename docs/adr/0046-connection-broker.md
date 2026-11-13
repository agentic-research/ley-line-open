# ADR-0046 — The connection broker: the data plane for cloister's declared connections

**Status:** Proposed (2026-10-27). Acceptance of the whole ADR is the user's decision; see
*Open questions*. Decided so far: Q7 (2026-11-10; implemented by `ley-line-open-be3711`, and
D5's microVM channel shipped with it) and *Slice 1* (Accepted 2026-11-11, which answers Q1,
Q3 and Q8 for the credential-free relay; being implemented, and no broker ships yet). D3,
D4 and Q2, Q4, Q5, Q6 remain proposals.
**Bead:** `ley-line-open-e2c5ce`
**Related:**
- cloister ADR-0069 (Proposed; bead `cloister-77870d`): declared connections, the declaration half
- ADR-0035 (one confinement manifest, attested enforcement; §9 the daemon is in the TCB)
- ADR-0036 O2 (§6 delivered on both tiers; the vsock port derivation)
- ADR-0037 (`network.connectLocal`; "a loopback port is whoever bound it first")
- ADR-0038 (Proposed): proxied credentials enter at a host-side boundary, not the confined client
- `schema-spec/confinement/v1` §3, §4, §6, §7 rule 5 and "Meet" (lists landed via `ley-line-open-e2c33f`, PR #494)
- `schema-spec/credential-isolation/v1` (injection strategies, receipt commitment)

---

## Context

Cloister ADR-0069 lets an external bundle declare its outbound wiring:

```toml
[[connections]]
from        = "orders"
name        = "orders-db"
appProtocol = "art.postgres"
[connections.target.local]
bundle = "postgres"
port   = "pg"
[connections.target.gcp]
cloudSql   = "proj:us-central1:orders"
vaultSlice = "orders-db-iam"
```

The service reads `DATABASE_ADDR` and dials it. Cloister resolves `target.<env>` at emit
time and derives the confinement grants. Something has to listen at the address in
`DATABASE_ADDR`, dial the resolved target, and supply the credential so the service never
holds it. ADR-0069 assigns that half to LLO (ADR-0035 boundary: policy in cloister,
mechanism in LLO). It does not exist.

What LLO has today:

| Piece | State |
|---|---|
| confinement/v1 §6 `unixSocket.allow` | Shipped. Seatbelt per-path on native macOS; vsock↔UNIX mappings on the microVM tier (ADR-0036 O2), and a grant carrying `guestPort` is presented to the workload as TCP on the guest's loopback (Q7, libkrun patch 0010; macOS only); refused on native Linux (Landlock ≤ ABI 7 has no AF_UNIX right). |
| confinement/v1 §3 `network.connectLocal` | Shipped, list-valued since #494. Landlock `ConnectTcp` on Linux, `network-outbound (remote tcp "localhost:N")` on macOS; refused on the microVM tier. |
| confinement/v1 §3 `network.allowHosts` | Refused on the native tiers ("route host-scoped egress through the proxy"). |
| A proxy that §3 and §5 point at | None. |
| credential-isolation/v1 | A cloister-hosted HTTP credential proxy (`/vault/proxy/<service>/<path>`) with a closed injection-strategy union and a receipt shape. HTTP only. |

So confinement/v1 already names the channel to a proxy (ADR-0037) and refuses host-scoped
egress in favour of one. This ADR names the proxy.

---

## Proposal

### D1. Placement: one broker process per run, on the host, outside the workload's boundary

The broker is a host-side process that the runtime spawns for a run whose grant carries a
connections document (D2). It is started, bound and ready **before** the workload's
confinement is compiled, and it is torn down with the run.

| tier | broker runs | workload reaches it through |
|---|---|---|
| native macOS | host process, sibling of the workload | §6 `connect` grant on the broker's socket (D5) |
| native Linux | host process, sibling of the workload | §3 `connectLocal` only; §6 is refused at this ABI (D5) |
| microVM | host process, outside the VMM | §6 grant compiled to a vsock↔UNIX mapping; a guest-side relay presents it to the workload (D5) |

Why per run, on the host:

- **Not in the daemon.** The broker parses HTTP/2, the Postgres startup protocol and the
  MySQL handshake from an untrusted workload, and holds live credentials. ADR-0035 §9
  already records that the daemon holds the CAS root, applies confinement, and will hold
  the domain's trust root. Putting a multi-protocol parser for workload-controlled bytes in
  that process widens the TCB in the place where a compromise costs the most.
- **Not in the guest.** A broker inside the microVM holds the credential inside the
  workload's boundary. The guest is one trust domain; anything in it can read the broker's
  memory. That defeats the purpose ("the service never holds it").
- **Not one shared broker per daemon.** A shared broker co-locates the credentials of every
  run in one address space, separated only by the broker's own correctness. A per-run
  broker holds exactly the credentials its run's grant authorized, and its lifecycle is the
  run's: the same teardown that kills the workload (process group / `cgroup.kill` / VMM
  exit) kills the broker, so no credential outlives the run.
- **The §6 ordering rule requires it.** confinement/v1 §6 refuses a `connect` grant whose
  endpoint is not yet bound, because the path is resolved at compile time. A broker started
  by the run supervisor before confinement compiles satisfies that rule by construction.

The broker is confined by an **LLO-internal profile**, the way the VMM is confined by the
outer nono profile (ADR-0035 §4 layer 1): bind on its own endpoint sockets, outbound to its
targets, nothing else on the filesystem. It is not a workload confinement/v1 document,
because it is TCB-side for the credentials it holds (ADR-0038's "host-side shim" row) and
needs modes the workload tiers refuse (e.g. §6 `bind` on native). Its egress cannot be
host-scoped on the native tiers for the same reason §3 `allowHosts` is refused there:
Landlock scopes TCP by port, not host. The broker enforces the host allow-list itself, as
the policy point §3 delegates to, and the receipt (D4) records each upstream it dialled.

The per-run cost is one extra process and its startup time per run. A run with no
connections pays nothing.

### D2. The input contract: `schema-spec/connections/v1`

Cloister emits, per bundle and per environment, one **connections document** alongside the
confinement manifest. It is specified the way confinement/v1 is (ADR-0035 §8):
JSON-Schema-first, canonical JSON, BLAKE3-256 over the canonical bytes, test vectors with
pinned digests.

Proposed shape (illustrative; the fields marked † depend on open questions):

```json
{
  "version": "cloister/connections/v1",
  "connections": [
    {
      "name": "orders-db",
      "appProtocol": "art.postgres",
      "endpoint": {"unix": "/run/llo/broker/orders/orders-db.sock"},
      "target": {
        "cloudSql": {"instance": "proj:us-central1:orders", "database": "orders"}
      },
      "credentialRef": {"gcpWorkloadIdentity": {"serviceAccount": "orders@proj.iam.gserviceaccount.com", "token": "access"}}
    },
    {
      "name": "payments",
      "appProtocol": "art.grpc",
      "endpoint": {"unix": "/run/llo/broker/orders/payments.sock"},
      "target": {"https": {"url": "https://payments-xyz.a.run.app"}},
      "credentialRef": {"gcpWorkloadIdentity": {"token": "id", "audience": "https://payments-xyz.a.run.app"}}
    }
  ]
}
```

- **`endpoint`** † is `{"unix": path}` or `{"tcp": port}` (D5). Cloister allocates it
  deterministically at emit time, as ADR-0069 §2 requires. It is the address the derived
  env var carries.
- **`target`** is a closed union of resolved targets: `{"tcp": {host, port, tls?}}`,
  `{"unix": path}`, `{"https": {url}}`, `{"cloudSql": {instance, database?, branch?}}`.
  Cloister resolves `target.<env>` and a sibling-bundle reference to one of these before
  emitting; the broker never sees an environment name. `cloudSql` stays symbolic because
  connecting to Cloud SQL is a protocol (Admin API lookup, ephemeral client certificate),
  and the protocol is mechanism. `database` and `branch` are structured components, never
  a DSN (cloister ADR-0051); the broker composes Dolt's `orders/dev-alice` form, not the
  operator.
- **`credentialRef`** † is a reference, never a value: `{"vaultSlice": name}` or
  `{"gcpWorkloadIdentity": {...}}` (D4). Absent means the connection carries no credential.
- **`appProtocol`** is closed: `art.http`, `art.grpc`, `art.postgres`, `art.mysql`,
  `art.redis`, `art.tcp`. `art.postgres`, `art.mysql` and `art.redis` wait on cloister's
  ADR-0030 §A4 amendment (ADR-0069 §2).

Canonical form, extending confinement/v1 §7 rather than inventing:

1. Rules 1–4 of confinement/v1 §7 apply verbatim (UTF-8 without BOM, ASCII-sorted keys at
   every level, two-space indent, no trailing newline, `null` omitted).
2. `connections` is sorted by `name` in byte order. A one-element list stays a list: unlike
   confinement/v1 §7 rule 5 there is no pre-list document whose digest must be preserved.
3. An empty `connections` list is refused; a bundle with no connections emits no document.
4. Refused at parse, never merged: two connections with one `name`; two connections with
   one `endpoint`; a `credentialRef` on `art.tcp`; `branch` on a target whose wire has no
   versioned engine; any field carrying a credential value.

**Cross-document rule.** The connections document and the confinement manifest are
checked against each other at run start, and a mismatch refuses the run naming the
connection:

- every `endpoint` appears as a channel grant in the workload's manifest (a §6 bare-string
  `connect` entry for `unix`, a §3 `connectLocal` port for `tcp`), and every broker-owned
  channel grant in the manifest has a connection;
- on the microVM tier the §6 vsock port is a pure function of §6 document order
  (ADR-0036 O2), and §6 `allow` is not re-sorted by §7, so cloister emits broker endpoints
  in §6 in connection-name order;
- a confinement **meet** (confinement/v1 §7 "Meet") that drops a `connectLocal` port drops a
  connection's channel. The run is refused, naming the connection, rather than started with
  a connection that cannot be dialled.

**Authorization.** † The document configures a credential-holding process, so it must be
authorized, not ambient. The proposal is an additive `RunGrant` field carrying its
BLAKE3-256 `connectionsDigest`, checked by the runtime before the broker starts, on the
model of `confinementDigest @8`. That is an execution/v1 wire addition and goes through the
cross-runtime contract review when it is made.

Test vectors, matching confinement/v1's layout: a canonical two-connection document; the
sort; each refusal in rule 4; a cross-document pair that matches and one that does not;
digests pinned in `CONNECTIONS_DIGESTS.blake3`, bytes in `VECTORS.sha256`.

The skeleton spec is **not** added in this change. Its `endpoint` and `credentialRef`
unions are exactly the open questions below, and a pinned vector would commit a digest to
a shape before the user decides it. Tracked as `ley-line-open-e9d940`.

### D3. Per-`appProtocol` handling

The broker handles each wire at one of two levels.

**Request level: `art.http`, `art.grpc`.** The broker terminates the workload's plaintext
HTTP/1.1 or HTTP/2 on the local channel and originates a verified TLS connection to the
target.

- The credential is injected per request using credential-isolation/v1's closed
  injection-strategy union (`authorizationBearer`, `authorizationBasic`, `headerNamed`,
  `queryParam`, `bodyField`), including its rule that an incoming `Authorization` is
  dropped. The broker reuses that spec's vectors as its conformance tests rather than
  defining a second union.
- `:authority`/`Host` is rewritten to the target; hop-by-hop headers are stripped.
- gRPC is HTTP/2 with trailers. The broker forwards trailers, records `grpc-status`, and
  audits per method: the `:path` is `/package.Service/Method`.
- One receipt per request, shaped like credential-isolation/v1's receipt commitment and
  carrying its MUST-NOT list (no credential in any form, no bodies, no query string).

**Connection level: `art.postgres`, `art.mysql`, `art.redis`, `art.tcp`.** The broker
participates in connection setup only, then splices bytes in both directions. It does not
parse queries.

- **Postgres.** The broker answers the workload's `SSLRequest`/`GSSENCRequest` with `N`
  (the local channel is not TLS), reads the `StartupMessage`, opens the upstream with TLS,
  authenticates with the injected credential (SCRAM-SHA-256, or the IAM token as the
  password for Cloud SQL IAM auth), relays `ParameterStatus`, `BackendKeyData` and
  `ReadyForQuery`, and then splices. `CancelRequest` arrives on a separate connection with
  the backend key and is forwarded to the same upstream.
- **MySQL.** The server speaks first, so the broker connects upstream **before** answering:
  it presents the upstream's capability flags in its own `HandshakeV10`, accepts the
  workload's `HandshakeResponse` without a password, authenticates upstream
  (`caching_sha2_password` or `mysql_native_password`; Cloud SQL IAM auth needs the
  cleartext plugin over TLS), sets the database (Dolt: `database/branch`) from the
  document, then splices.
- **Redis.** `AUTH` (and `SELECT` if the target names a database) is sent upstream at
  connection setup, then splice.
- **Raw TCP.** Splice only. No credential can be injected into an opaque stream, so a
  `credentialRef` on `art.tcp` is refused at parse (D2 rule 4).
- One receipt per connection: target, bytes each way, duration, close reason.

Dolt speaks the MySQL wire and DoltgreSQL the Postgres wire, so engine choice never reaches
the broker (ADR-0069 §3). ADR-0069's rail that SQL-wire connections are declared only from
external bundles stays cloister's; the broker refuses nothing on that ground and does not
need to.

The connection-level handlers are where the work is. Request-level proxying is a solved
problem with existing libraries; credential injection into a database handshake is not,
whichever proxy framework is chosen (D6).

### D4. Credential injection

Policy stays in cloister, mechanism in LLO:

| Concern | Owner |
|---|---|
| which bundle may use which credential, which GCP principal a bundle maps to, which scopes | cloister (emitted as `credentialRef`; the GCP-side IAM binding lives in the operator's infrastructure) |
| holding static secrets | cloister's CredentialVault (`vaultSlice`) |
| the bundle's identity | notme authority, Interlace/WIMSE lane 2 (ADR-0035 identity table) |
| performing a token exchange, refreshing tokens, injecting them into a wire | **the broker (LLO)** |

**GCP: the broker performs the exchange.** For `gcpWorkloadIdentity`, the broker exchanges
a subject token naming the bundle's notme identity for a federated token at Google STS
(RFC 8693 token exchange), then, if `serviceAccount` is named, impersonates it through IAM
Credentials (`generateAccessToken` for an access token, `generateIdToken` for an ID token).

Why the broker and not cloister or the workload:

- **The token type is a property of the wire, and the wire is the broker's.** Cloud SQL IAM
  database auth takes an OAuth access token as the password at connection setup; Cloud Run
  invocation takes an ID token whose audience is the service URL, per request; Google APIs
  take an access token per request. Choosing and placing the token is protocol handling.
- **Refresh follows the connection lifecycle.** Tokens last about an hour; a pooled SQL
  connection outlives one, a request does not. The component that owns connection
  lifetime owns refresh. A cloister-side exchange would have to push fresh tokens into a
  running broker before expiry, adding a second live channel for no gain in authority.
- **The token never enters the workload.** The workload-side alternative (Google client
  libraries with an `external_account` credential file) works with no broker, and is
  exactly what ADR-0069 rules out: the service would hold the token.
- **The exchange is mechanism.** RFC 8693 is a generic protocol. Every policy input — the
  audience, the principal, the scopes — arrives from cloister in `credentialRef`, and the
  IAM binding that decides whether the exchange succeeds lives outside LLO.

The subject token itself is lane-2 identity, not LLO's to mint. How it reaches the broker,
and how it is refreshed for a run that outlives it, is open question Q4.

**Static secrets (`vaultSlice`).** credential-isolation/v1 property 1 says plaintext never
crosses the vault's response boundary: the vault makes the upstream call. That works for
`art.http` and cannot work for a database handshake, which the vault does not speak. Two
consistent choices exist (Q5): the broker relays HTTP through the vault proxy and holds a
plaintext lease only for connection-level wires, or it holds a lease for every wire. Either
way the broker is inside the custody boundary for what it injects, as ADR-0038 already
places a host-side shim ("transiently, in memory").

### D5. The workload's channel to the broker

The two channel dimensions differ in the one property a credential-injecting proxy depends
on: whether the broker can tell who connected.

| | §6 UNIX socket | §3 `connectLocal` loopback TCP |
|---|---|---|
| peer identity | filesystem: the socket sits in a per-run directory (mode 0700, owned by the run's uid); `getpeereid`/`SO_PEERCRED` gives the peer's uid | none: any local process may connect to a loopback port, and any process may bind it first (ADR-0037) |
| what a stranger gets | nothing; it cannot reach the path | **the run's credentials**: the broker injects them for whoever connects |
| native macOS | enforced per path (Seatbelt) | enforced per port |
| native Linux | **refused** at Landlock ≤ ABI 7; becomes available with ABI 9 `RESOLVE_UNIX` without a spec change | enforced per port |
| microVM | delivered as vsock↔UNIX mappings, one per grant | **refused** (a guest's loopback is its own) |
| unmodified client | Go: `pgx` (`host=/dir`), `go-sql-driver/mysql` (`unix(/path)`), `grpc-go` (`unix:///path`) and `go-redis` (`Network: "unix"`) take a socket; `net/http` needs a custom `DialContext` | any TCP client |
| N connections | N bare-string §6 entries, never a directory grant (the microVM tier refuses directory grants by name, and a directory admits other runs' sockets) | N `connectLocal` ports, list-valued since #494; sorted, met by intersection (confinement/v1 §7) |

**Recommendation: §6 UNIX sockets are the broker channel, and a credential-bearing
connection is never offered over loopback TCP on the host.**

- **microVM (recommended production tier).** The broker binds host sockets; §6 compiles
  them to vsock mappings. A guest-side relay, part of LLO's guest init, listens on the
  guest's loopback and forwards each port to its vsock mapping. This keeps the unmodified
  TCP client: inside the VM, loopback is single-tenant, so the VM boundary authenticates
  the peer that the host loopback cannot. The relay shipped as
  libkrun patch 0010 (Q7, `ley-line-open-be3711`).
- **native macOS.** §6 sockets in a per-run 0700 directory, the broker checking the peer
  uid. Clients that cannot dial a socket (`net/http` without a dialer) need a code change
  or the microVM tier.
- **native Linux.** §6 is refused, and `connectLocal` hands the run's credentials to any
  local process. A connection with a `credentialRef` is refused on this tier, naming the
  tier and the connection; a connection without one may use `connectLocal`. What, if
  anything, relaxes that is Q2.

The `endpoint` union in D2 keeps both forms so `art.tcp` and credential-free connections on
native Linux have a spelling, and so §6 on Linux can be adopted when the Landlock stack
reaches ABI 9 without a contract change.

### D6. Implementation: hyper and owned handshake handlers, not Pingora

| | Pingora | Envoy | hyper 1.x + owned handlers |
|---|---|---|---|
| what it is | Cloudflare's Rust proxy framework | C++ proxy, external binary | the HTTP library already in the dependency graph (via `axum`, `reqwest` 0.13 / `hyper-rustls`) |
| platforms | Linux tier 1; macOS best-effort (upstream README) | Linux and macOS | wherever tokio runs; LLO's tiers are macOS native, Linux native and microVM on both |
| TLS | OpenSSL, BoringSSL, s2n; rustls experimental (upstream README) | BoringSSL | rustls, already used |
| process model | a server framework that owns the process: its own runtimes, graceful upgrade, reload | own process, configured over xDS or files | a library inside a process LLO already supervises |
| HTTP/gRPC | yes, the framework's strength | yes | hyper + `h2` (trailers) |
| DB credential injection | none: its L4 path moves bytes | protocol filters exist for observability; none performs the auth handshakes D3 needs | owned, as for every option |
| new external binary | no | **yes**, the class ADR-0019 and the execution goal exclude | no |

**Recommendation: hyper.** Pingora's advantages (connection pooling and graceful upgrade
at edge scale) serve a long-lived shared proxy handling very many connections. The D1
broker serves one run's handful of connections and dies with it. Pingora's costs land on
LLO's requirements: macOS is best-effort upstream while it is a first-class LLO tier,
rustls is experimental while the repo is on rustls, and the framework wants to own the
process lifecycle that the runtime supervisor already owns. And the hard part, D3's
connection-level handshakes, is owned code under every option. Envoy fails the
external-binary bar outright.

Revisit Pingora if the broker becomes shared (Q1 answered "per daemon"), at which point
pooling across runs is worth a framework. Spike exit criteria before this is accepted: a
hyper broker serving one HTTP/2 gRPC call with trailers and one Postgres SCRAM handshake
splice, on macOS and Linux, under the D1 profile.

### D7. Threat model and fail-closed rules

Assets: the credentials the broker holds; the authority to reach targets; the receipts.

| Threat | Mechanism | Rule |
|---|---|---|
| another local process uses the run's credentials | connects to the broker's loopback port | credential-bearing connections only over §6 or the microVM relay (D5); peer uid checked on §6 |
| port or path squatting | a process binds the endpoint before the broker | the broker binds in a per-run 0700 directory before confinement compiles; §6 resolves the path at compile time; a pre-existing path refuses the run |
| workload reads a credential | the broker echoes it in an error, a header, a log | credential-isolation/v1 MUST-NOT list applies to every broker output; reserved response headers stripped; error bodies constant-shape |
| workload steers the broker to an arbitrary host | `Host` header, CONNECT, absolute-form URI, redirect | the target is fixed per connection by the document; the broker rewrites authority, refuses CONNECT and absolute-form, does not follow redirects |
| workload smuggles a second credential use | HTTP request smuggling across a reused upstream connection | upstream connections are not shared across endpoints; HTTP/1 framing ambiguities (`Content-Length` with `Transfer-Encoding`) are refused, not normalized |
| connection document tampered between issuer and broker | config is ambient | `connectionsDigest` in the grant, checked before the broker starts (D2, Q3) |
| a declared connection silently not delivered | a tier cannot express the channel; a meet drops a port | the run is refused naming the connection; never started with fewer connections than declared (confinement/v1 §9 condition 6, one level up) |
| upstream impersonation | DNS or network attacker | TLS to every non-local target with verification; no `insecureSkipVerify` spelling exists in the contract |
| credential outlives the run | broker survives teardown | the broker is in the run's process group / cgroup / supervised lifecycle; tokens held in memory only |
| broker compromise | parser bug in workload-controlled bytes | the broker holds one run's credentials, not the domain's; its LLO-internal profile denies the filesystem and the daemon's sockets; it is not the daemon (D1) |
| `SCM_RIGHTS` over §6 | the peer passes descriptors | the broker never accepts or sends ancillary data (confinement/v1 §1) |

Fail-closed rules, in one place:

1. Any refusal from D2 rule 4 or the cross-document rule refuses the run before the
   workload starts.
2. A token exchange or vault lease that fails refuses the **connection** at setup with a
   constant-shape error; the broker never forwards an unauthenticated request in place of
   an authenticated one.
3. A connection whose tier cannot deliver its channel refuses the run, naming tier and
   connection.
4. The broker failing to become ready aborts the run on the existing failed-start path.
5. A broker exit during the run is a run failure, not a degraded run.

---

## Slice 1: credential-free relay (v0.30.0)

**Status of this section: Accepted (owner, 2026-11-11).** The owner accepted the full
design, S0–S12, with the answers recorded in S13. It builds on Q7 (decided 2026-11-10,
implemented by `ley-line-open-be3711`). The constraints cloister supplied as the consumer
(2026-11-11) are marked *(cloister)*; where this section departs from one, it says so and
why. ADR-0046 as a whole stays Proposed for D3 and D4.

### S0. What the slice is for, and what it is not

Cloister's demo: an agent in a libkrun microVM on macOS dials `127.0.0.1:<guestPort>` and
reaches an API that runs as a Service in a kind cluster on the same Mac. The guest half
ships: patch 0010 presents a §6 `connect` grant carrying `guestPort` as TCP on guest
loopback, and the VMM joins it over vsock to a host UNIX socket (Q7). VM-to-VM already works
with no broker, by two VMs sharing one bind path (cloister live run, comment on
`ley-line-open-e2c5ce`, 2026-11-10). What is missing is the host-side process that listens
on that UNIX socket and joins each connection to one allow-listed TCP `host:port`.

Slice 1 is that process and nothing more: it copies bytes. In scope are D1 (per-run host
broker), a minimal D2, D5 for the microVM tier on macOS (and native macOS, S6), and D7's
fail-closed rules as they apply to a relay that holds no credential.

Out of scope and deferred, unchanged: D3 (per-`appProtocol` handling beyond opaque bytes),
D4 (credential injection), Q2 (native Linux with credentials), Q4 (subject token), Q5
(static secrets). A slice-1 broker has no TLS client, no parser and no credential.

### S1. Q1 — per-run broker (proposed)

One broker process per run whose grant carries a connections document; none otherwise.

- **Its OS profile can name exactly one run's upstreams.** The broker's LLO-internal
  profile allows outbound TCP to the declared upstream ports and nothing else (S5). A
  per-daemon broker's profile would be the union of every concurrent run's upstreams, so
  each run's broker would hold reach that only some other run was granted. That widens
  reach, which default deny forbids.
- **The lifecycle is the run's.** "No channel outlives its run" (Q7 invariant I9) holds by
  construction when the broker is torn down with the run (S7). A shared broker must
  instead remove one run's listeners from a live process, which is state that can drift.
- **Failure is isolated.** A broker exit fails one run (D7 rule 5), not every run on the
  daemon.
- **D4 inherits it.** When credentials arrive, a per-run broker holds exactly one run's
  (D1). Choosing per-daemon now would have to be undone then.

The cost is one process and its startup per run with connections. D6's argument for
revisiting Pingora applies only to a shared broker, so it does not arise.

### S2. The minimal `connections/v1` document (proposed)

Slice 1 delivers a strict subset of D2. Every slice-1 document is a valid D2 document, so
D3/D4 extend the schema without changing the digest of any document slice 1 accepts.

```json
{
  "connections": [
    {
      "appProtocol": "art.tcp",
      "endpoint": {
        "unix": "/private/tmp/cloister-ch-7Qm2/orders.api.sock"
      },
      "name": "orders-api",
      "target": {
        "tcp": {
          "host": "127.0.0.1",
          "port": 30080
        }
      }
    }
  ],
  "version": "cloister/connections/v1"
}
```

Canonical form: D2's rules 1–3 (confinement/v1 §7 rules 1–4 verbatim; `connections`
sorted by `name` in byte order; a one-element list stays a list; an empty list is
refused), plus two rules a second implementation needs to reach the same bytes:

- **Strings are printable ASCII.** Every string value is in U+0020–U+007E and contains no
  `"` and no `\`, so no JSON escape is ever emitted and serializers that differ on escaping
  (raw UTF-8 against `\uXXXX`) produce the same bytes. A string outside that set (a NUL, a
  control character, a non-ASCII path) is refused at parse.
- **Numbers are JSON integers.** `port` is written as a decimal integer with no sign,
  fraction, exponent or leading zero; `30080.0`, `"30080"` and `3.008e4` are refused.

`connectionsDigest` is BLAKE3-256 over those canonical bytes, carried as a `DigestRef`
with `algorithm` = `blake3-256` and `value` = 64 lowercase hex characters (the form
`read_digest` in `authorization.rs` accepts). It names the connections document's bytes
and nothing else; it is not `confinementDigest`, a blob hash or any arena or store root.
The two digests are both plain BLAKE3 over canonical JSON; the in-document `version`
string and each parser's refusal of the other's keys keep them apart.

Field rules (each refusal names the connection and the field):

| field | slice-1 rule | refused at parse |
|---|---|---|
| `version` | exactly `cloister/connections/v1` | any other value |
| any object | only the keys shown | an unknown key at any level (D2 extends the schema; a slice-1 runner never ignores a key it does not deliver) |
| `name` | `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`, unique | a duplicate |
| `appProtocol` | `art.tcp` only | any other closed-set value, named as "not delivered in slice 1"; any value outside the set |
| `endpoint` | `{"unix": path}` only | `{"tcp": port}` (S6); any other key |
| `endpoint.unix` | absolute; no `.` or `..` component, no `//`, no trailing `/`; at most 103 bytes; unique across connections | each violation, by name |
| `target` | `{"tcp": {host, port}}` only | `unix`, `https`, `cloudSql` (D2 union members slice 1 does not deliver); a `tls` key |
| `target.tcp.host` | a loopback IP literal: IPv4 dotted-quad in `127.0.0.0/8` (four decimal octets, no leading zeros), or exactly `::1` (that spelling only) | any name, including `localhost`; any non-loopback address, named "non-loopback target"; any other spelling of a loopback address (`127.000.0.1`, `0:0:0:0:0:0:0:1`, `::ffff:127.0.0.1`) |
| `target.tcp.port` | a JSON integer, 1–65535 | 0, out of range, or not an integer |
| `credentialRef` | absent | present (D2 rule 4 already refuses it on `art.tcp`) |

`appProtocol` is required although slice 1 accepts one value, so that a slice-1 document
never relies on a default a later slice would have to keep. 103 bytes is the smallest
`sun_path` capacity among the supported platforms (macOS: 104 including the NUL), so a
vector's validity does not depend on the host that checks it *(cloister: at most 103)*.

**Why loopback IP literals only.**

- **No name is resolved, ever.** With a literal there is no resolver to poison and no
  answer that changes between check and dial (DNS rebinding). The dialled address is the
  committed bytes, so the receipt records exactly what the digest authorized
  *(cloister: no DNS, no re-resolution)*.
- **No TLS in slice 1, so no network hop.** D7 requires verified TLS to every non-local
  target and offers no spelling to skip it. Slice 1 has no TLS client, so the only targets
  it can serve within D7 are local. Non-loopback is refused by name rather than relayed in
  plaintext *(cloister: refuse non-loopback by name)*.
- **The OS profile adds a second layer on loopback.** Seatbelt scopes outbound TCP as
  `remote tcp "localhost:N"`: by port, on loopback, not by address. So the broker's
  sandbox holds it to the declared ports on loopback, and the broker's code holds it to
  the one address (S5). For a non-loopback host the profile could only scope by port, and
  the code would be the sole enforcement.

**The schema represents the loopback range; the runtime delivers `127.0.0.1` (S-Q3,
decided).** The document type and canonical form above already accept every loopback
literal, `127.0.0.0/8` and `::1`, with pinned vectors, so widening never changes the schema
or any digest. Delivery is a separate runtime check, run at start before anything is
spawned: slice 1 delivers only `127.0.0.1`, the address kind publishes on and the one
`localhost` in a Seatbelt filter is known to match. Any other loopback target is refused
by name, "loopback address not delivered by this runtime" (`unsupportedBackend`), because
a document the broker's profile then blocked would be a declared connection that never
dials. The delivered set is one constant beside the broker profile; widening it is a gate
flip that the loopback probe (S11a) justifies when it passes for that address on every
delivering platform.

**Cross-document rule, slice 1.** Checked at start, before the broker is spawned; a
mismatch refuses the run naming the connection and the grant:

- every `endpoint.unix` appears exactly once in the confinement manifest's §6 `allow` as a
  connect grant on that exact path: on the microVM tier the object form
  `{"path", "mode": "connect", "guestPort"}`, on native macOS the bare string;
- no §6 grant that can bind (`bind` or `connect-bind`) equals an endpoint path or is a
  directory grant containing one, and no §2 `fs.allow` grant with write access covers an
  endpoint's parent directory. Either would let the workload unlink the broker's socket
  and bind its own in its place (the workload never owns the broker's socket);
- a §6 connect grant that names none of this run's endpoints is not this run's broker's.
  It must not name, and a directory grant must not contain, an endpoint of **another live
  run** (S4, "endpoint registry"): the VM-to-VM shared-path joins of S0 keep working
  between workload-owned paths, but no run reaches another run's broker.

The guest address stays where Q7 put it: `guestPort` in the confinement manifest, committed
by `confinementDigest`. The connections document does not repeat it.

### S3. Q3 — authorizing the document (proposed wire change)

**Additive `RunGrant` fields, on the `confinementDigest @8` / `confinementManifest @15`
model:**

```capnp
connectionsDigest   @16 :DigestRef $Traits.optional;  # blake3-256 over canonical connections/v1
connectionsDocument @17 :Text      $Traits.optional;  # the canonical document the digest is taken over
```

`@16` carries the authority, `@17` the document. Carrying only the digest repeats the defect
`@15` fixed: a runner could verify a match without learning what was authorized. The
runner recomputes the digest from `@17`, requires it to equal `@16`, and requires `@17` to
be byte-equal to its own canonicalization (a non-canonical document is refused, never
normalized).

**Present** means: `@16` has a non-null pointer, and `@17` has a non-null pointer to
non-empty text. A non-null `@16` with an empty `algorithm` or `value`, or a non-null empty
`@17`, is malformed and refused, never read as absent (existing manifest code treats empty
`@15` text as absent; these fields do not inherit that reading).

**Signing bytes.** Both fields enter `signing_bytes = canonical(RunGrant with signature
cleared)` (`wire/run-grant.md`) with no change to that rule: they are pointer fields in the
signed struct, so a signature covers them. A grant that sets neither has null pointers in
those slots, which canonicalization truncates, so its signing bytes are byte-identical to
those of the same grant before the fields existed. Existing signatures stay valid.

**What an old daemon does, and why a new field alone is not enough.** A shipped daemon's
`capnp_json` decoder walks the schema's fields, so it drops the unknown
`connectionsDigest` and `connectionsDocument` keys (`transport.rs`;
`refuse_unknown_limit_keys` documents the same gap for limits). An in-process embedder that
passes binary bytes keeps the unknown pointers but reads nothing from them. Either way an
old daemon reads no connections document. What it does next depends on how it verifies:

| old daemon configuration | what refuses the grant | error |
|---|---|---|
| `--evidence-store … --trust-key …` (production), JSON transport | the signature check, which runs first: the daemon rebuilds the grant without `@16`/`@17`, so its signing bytes differ from what the issuer signed | `unauthenticated`, "RunGrant signature is not by a trusted issuer" |
| `--allow-unverified-evidence`, or an in-process embedder passing binary bytes | the marker below | `unsupportedBackend`, "credential broker authority requires a broker integration" |
| no verifier flags | every start is refused | `unauthenticated` |

A production daemon thus refuses on the signature before it reads the marker. That
refusal is real but incidental, and it does not hold for an unverified daemon, so the
grant also carries a marker every shipped daemon refuses whatever its verifier:

- **`credentialBrokerRefs @13` = exactly `["cloister/connections/v1"]`.** Every daemon
  since execution/v1 first shipped (PR #317; the text is unchanged at v0.15.0, v0.22.0 and
  v0.29.0) refuses a non-empty `credentialBrokerRefs` with `unsupportedBackend`:
  "credential broker authority requires a broker integration" (`authorization.rs`). The
  check runs before the resolver and the backend, so nothing is spawned. The entry names
  the contract, not a credential, and is inside the signing bytes.
- **This gives `@13` a stated meaning, not a second one.** Today the field is documented as
  "Resolved credential brokers; never credential values", and its only reader is that
  refusal; the canonical vector carries a broker reference (`broker://claude/paid`). The
  same change rewrites the field's `$Traits.doc` and the execution/v1 README to define its
  entries as **the broker integrations the run requires, each named by contract id or
  broker reference**; reserves `cloister/connections/v1`; and states that a daemon refuses
  any entry it does not implement. D4 then adds credential-bearing entries under the same
  meaning: a D4 grant carries the marker plus the brokers it names, and the "exactly the
  marker" rule below becomes "the marker plus entries D4 defines". Until then the marker
  plus a `broker://` entry is refused, which fails closed.
- **The new daemon's rule is all-or-nothing.** `@16`, `@17` and the marker are present
  together or absent together; any other combination is refused with `invalidGrant`,
  naming the missing part. `credentialBrokerRefs` holding anything other than exactly the
  marker is still refused as today.

This is the critical-extension pattern (X.509 `critical`) built from a field old readers
already treat as critical. The rejected alternative, folding the connections into the
confinement manifest, also gets an old-daemon refusal (an unknown confinement/v1 dimension
is refused) but puts upstream ports into `confinementDigest` and so into the workload
identity (§8), mixing a broker's configuration into the workload's confinement, as Q3
already notes.

**Capability discovery.** `llo_execution_capabilities` adds the entry
`{name: "cloister/execution/v1/connections", version: "1"}` (constant beside
`RESOURCE_LIMITS_CAPABILITY` in `transport.rs`), advertised by a daemon that reads `@16`/`@17`
and has a broker, whatever its tier; a tier that cannot serve a connection refuses at start
by name (S6). A client MUST find the entry before sending a grant with connections. Its
absence means a daemon older than the first LLO release that ships this slice, and the
client's refusal names that minimum version *(cloister: as the `casRoot` refusal does)*.
The old daemon's own refusal is one of the fixed texts in the table above; it cannot name
a version it predates, so the version is the client's to state. The version string `"1"`
follows the revision convention of `cloister/execution/v1/resource-limits` (`"2"`): it
numbers this extension's layout, not the execution/v1 contract (`"v1"`).

**Consumer surfaces.** The schema change touches:

- `execution.capnp` (normative); the Rust bindings are generated at build time from it and
  are not committed;
- `rs/ll-open/cli-lib/src/daemon/execution-tools.json`, regenerated by
  `task gen:execution-tools` (the `@13` doc text change regenerates it too);
- the execution/v1 README (the `@13` meaning, the capability table) and `wire/run-grant.md`;
- `test-vectors/` and `VECTORS.sha256`: a new pinned **connections grant vector**, a grant
  that sets `@16`/`@17` and the marker, with its canonical bytes and signing-bytes digest,
  checked by `leyline-schema-spec` and reproduced by cloister. This is the independent
  oracle for cloister's signer; without it producer and consumer only confirm each other;
- the published `execution.capnp` asset and the zod schemas built from it (S8 on receipt
  readers).

No Go execution/v1 consumer exists (`clients/go` mirrors only `ErrorCode`, which this change
does not touch), and `compatibility.json` carries no execution version. Cloister's emitter
must reproduce `connectionsDigest` byte for byte from the `connections/v1` vectors
(`ley-line-open-e9d940`) before it sends one *(cloister: conditional on those vectors)*.

### S4. Q8 — endpoint allocation (proposed: the issuer allocates, LLO validates)

The issuer chooses every endpoint path and commits it in the document, so in
`connectionsDigest`. LLO chooses no path and derives none *(cloister)*. Cloister's
convention is `<channelDir>/<bundle>.<portName>.sock` in a fresh, realpath'd, mode-0700
channel directory per run.

This answers both halves of Q8. The issuer predicts the path because it chose it; the
ADR-0036 equality contract needs nothing LLO derives. Two concurrent runs of one bundle do
not collide because each has its own channel directory. The path need not contain the run
id (which would be circular: `run_id` derives from the grant), only an issuer-chosen
component unique per run. And if an issuer does reuse a directory, the collision is a
refusal, not a shared socket:

Checked at start, after authorization and before the broker is spawned; each failure
refuses the run by name with the connection and the reason:

| check | refusal |
|---|---|
| the endpoint path does not exist (`lstat` gives `ENOENT`) | `resourceConflict`: "endpoint exists" (another run, a stale socket, a squatter) |
| the parent exists and is a directory | `invalidGrant`: "endpoint parent missing" |
| the parent's realpath equals the parent as written | `invalidGrant`: "endpoint parent is reached through a symlink" |
| the parent is owned by the daemon's effective uid and its mode is exactly 0700 | `invalidGrant`: "endpoint parent not private". The issuer and the daemon therefore run as one uid in slice 1; an issuer under another uid cannot create a directory the daemon accepts, and is refused by this name rather than accommodated |
| no live or starting run owns the endpoint or holds a §6 grant on it (the registry below) | `resourceConflict`: "endpoint held by another run" |
| the full path fits this platform's `sun_path` | `invalidGrant`: "endpoint path too long" (the 103-byte parse rule makes this unreachable on the supported platforms; it stays as the platform check) |
| no two connections share a path | refused at parse (S2) |

The broker binds every endpoint before the run reports ready, and before the workload's
confinement compiles, which §6's ordering rule requires of a connect grant *(cloister:
`up` waits for the socket to exist)*. Slice 1 allocates nothing on the issuer's behalf, so
`StartOutput` gains no field. The broker cannot unlink anything (its profile denies the
filesystem, S5); the supervisor unlinks the endpoints at teardown. After a supervisor that
was killed, the socket files stay, and the next run naming them is refused "endpoint
exists": stale nodes fail closed. The channel directory is the issuer's to remove.

Checking ownership and mode closes a gap the issuer's convention leaves to discipline: a
0700 directory owned by another uid, or reached through a symlink, would let a process
outside the run choose what the path resolves to between the issuer's check and the bind.
The checks and the bind are by path, not by directory descriptor; a same-uid process could
still swap the parent between them. That is the same-uid residual of S10; binding through
a directory descriptor (`bindat` where the platform has it) would narrow it.

**Endpoint registry.** The 0700 parent and a uid check stop other users, not other runs:
every run's VMM, and on native macOS every workload, runs as the daemon's uid. Without a
further rule, run B could carry a plain §6 connect grant (the S0 shared-path join) on run
A's endpoint, reach A's target, which B's grant never named, and have its traffic counted
in A's receipt. So the daemon keeps, under one lock, a registry of every live or starting
run's broker endpoints and of every §6 path (and directory grant) each live or starting
run's manifest names. At start, under that lock and before anything is spawned:

- a run whose §6 grant names, or whose directory grant contains, an endpoint another run
  owns is refused (`invalidGrant`, naming the path);
- a run whose endpoint another run's §6 grant already names or contains is refused
  (`resourceConflict`), which closes the ordering race in which B passes its check before A
  registers and compiles after A binds.

On the microVM tier the broker also checks the connecting peer's pid (`LOCAL_PEERPID` on
macOS) against the run's worker, which hosts the VMM, and closes any other peer without a
dial. The registry covers both tiers; the pid check is a second layer where the run has one
peer process.

*Implementation notes (2026-11-12, `ley-line-open-52b5b3`, from execution-boundary review):*

- **Registry: resolved paths.** The registry compares paths as the sandbox resolves them,
  not as they are spelled. nono canonicalizes a §6 grant before Seatbelt sees it, so both
  sides are resolved through the realpath of their longest existing ancestor, and
  case-folded on macOS. A non-ASCII §6 grant is refused at registration: a
  case-insensitive volume folds Unicode case, and a not-yet-bound endpoint cannot be
  folded through realpath. Endpoints are printable ASCII, so this never refuses a grant
  that could not name one.
- **Peer pid.** The broker is spawned before the worker exists (S7 step 3, then step 5),
  so on the microVM tier it starts with `--await-peer`. It refuses every peer until the
  supervisor names the worker's pid once over the control pipe (`{"op":"peer"}`), and the
  pid can never be changed.
- **Preflight.** A parent carrying an ACL is refused, and so is any ancestor that someone
  other than root or the daemon's uid owns, or that others can write without the sticky
  bit; either lets another user swap the parent.
- **Teardown.** `ready` reports each bound socket's device and inode, and teardown unlinks
  a path only while it is still that socket.

### S5. The relay: allow-list enforcement and failure behaviour (proposed)

**The broker dials exactly the committed target and nothing else.**

- Each listener is bound to one connection; the target is fixed when the broker starts
  from the authorized document, as a `SocketAddr` built from the literal. The broker reads
  no address from the stream, parses no protocol and follows nothing, so the workload has
  no way to steer it.
- One upstream dial per accepted connection; no pooling, no reuse across connections, no
  dial before a connection arrives.
- **The document arrives over the control pipe, and the broker attests it.** The
  supervisor writes the authorized canonical document to the broker's control pipe (not
  argv, not a file). The broker's ready report echoes the `connectionsDigest` of what it
  parsed, bound and confined to; the supervisor compares it with the grant's `@16` and
  refuses the start on a mismatch. This is the worker precedent, whose ready event echoes
  the applied confinement digest for the supervisor to compare (`backends/native.rs`, bead
  `ley-line-open-7a3109`).
- **Bind, then confine.** The broker binds every endpoint, then applies its LLO-internal
  nono profile to itself, then reports ready. Two layers enforce the allow-list: the
  broker's code dials one address per listener, and its sandbox refuses any other port.
  What nono emits on macOS is not "deny everything else": its base profile always allows
  `process-exec*`, `process-fork`, `mach-lookup` (minus keychain services), the
  mDNSResponder socket, `sysctl-read`, `ipc-posix-shm*` and `file-read*` on `/`. The broker
  profile is therefore nono's base plus explicit `platform_rule` denies, which nono emits
  after its allows so the deny wins: `process-exec*`, `process-fork`, the mDNSResponder
  path (no resolver, matching "no names"), `file-read*` and `file-write*` everywhere, and
  `mach-lookup` beyond the services the Rust runtime needs (the set is fixed by the spike).
  *Measured in implementation (2026-11-12, `ley-line-open-52b5b3`):* the mach-lookup
  allow-set is empty, and the mDNSResponder deny cannot take effect. nono 0.71 emits its
  network block, which allows that socket for DNS, after every platform rule, so the
  socket stays reachable. Resolution is still refused (`getaddrinfo` fails under the
  profile through the `mach-lookup` and file-read denies), and the socket adds no reach.
  The residual is `ley-line-open-fb5c4b`.
  Outbound TCP is allowed to `localhost:<port>` for each declared port and nothing else;
  no inbound TCP, no TCP listen. Accepting on the already-bound UNIX listener needs no path
  right; S11's positive control proves the confined broker still accepts and relays.
- The peer is checked on accept: `getpeereid` must give the daemon's effective uid, and on
  the microVM tier `LOCAL_PEERPID` must be the run's worker (S4, "endpoint registry"). Any
  other peer is closed without a dial and counted in `foreignPeers` (S8).

**Failure modes.** A guest connection is accepted by the guest init relay before it reaches
the host (patch 0010), so a guest client never sees `ECONNREFUSED` from a declared port; it
sees bytes or end-of-file *(cloister: refused or EOF)*. One exception: a guest that has
written bytes the relay never read when the relay closes may get a reset from the guest
kernel, and then sees `ECONNRESET` instead of end-of-file. Either way no byte arrives.

| event | broker behaviour | the guest sees |
|---|---|---|
| upstream `ECONNREFUSED` (nothing listening yet, e.g. kind still starting) | holds the accepted connection and retries, backing off from 5 ms to 200 ms, for at most 10 s, ending early when the guest side hangs up; the same bound as a serve channel's hold (patch 0010, #554) *(cloister)* | the connection works once the upstream is up within 10 s; otherwise end-of-file without a byte |
| any other dial error (`ECONNRESET`, `ETIMEDOUT`, `EHOSTUNREACH`, profile `EPERM`) | closes at once, no retry | end-of-file |
| upstream resets mid-stream | closes both directions | end-of-file (a UNIX stream peer on macOS observes no reset, and the vsock hop carries none) |
| half-close: one side ends its writes | `shutdown(SHUT_WR)` toward the other side; the other direction keeps copying until it ends | the same half-close |
| more concurrent connections than the run's cap (proposed: 256 per run) | closes the new connection without a dial; counted as a refusal | end-of-file |
| broker exits or crashes during the run | the supervisor fails the run (D7 rule 5): cancels the workload, terminal state `failed`, receipt written from what the supervisor holds | end-of-file on open connections; new dials end at once (nothing listens at the path) |

There is no unbounded retry anywhere: the 10 s hold is per connection and only for
`ECONNREFUSED`. Each direction has a fixed buffer (64 KiB). Nothing is sent with ancillary
data. *Measured in implementation (2026-11-12, `ley-line-open-52b5b3`):* `read(2)` alone does
not keep a passed descriptor out. macOS installs an `SCM_RIGHTS` descriptor in the receiving
process even on `read(2)` or `recv(2)` with no control buffer (Linux discards it). So the
client direction is read with `recvmsg(2)` and a control buffer, every received right is
closed at once, and a truncated control message ends the connection. The hold's hang-up
check uses `FIONREAD`, never a peek, so a descriptor passed during the hold is never read and
is released when the connection closes (D7, `SCM_RIGHTS`).

**Strength.** The guest side is capability-shaped: an undeclared guest port has no
listener (Q7). The host hop is `policy` at best *(cloister)*: the broker guarantees which
address it dials, not which process holds it, because a loopback port is whoever bound it
first (ADR-0037). Slice 1's obligation is never to widen past the committed target;
cloister's build check owns `minStrength`.

### S6. Tiers (proposed)

| tier | slice 1 | why |
|---|---|---|
| microVM, macOS | delivered | §6 connect with `guestPort` (Q7) is the channel; the broker listens at its path |
| native macOS | delivered | the same broker; the workload dials the §6 path itself (a bare-string connect grant, enforced per path by Seatbelt). A workload that cannot dial a UNIX socket needs the microVM tier |
| microVM, Linux | refused by name | `guestPort` is refused off macOS until it runs live there (Q7) |
| native Linux | refused by name | §6 is refused at Landlock ≤ ABI 7; `endpoint {"tcp"}` (D5's `connectLocal` form) is not delivered in slice 1 |

Refusal is `unsupportedBackend`, naming the tier and the connection, raised by the service
before anything is spawned (S7 step 2; D7 rule 3). This is a new check: the existing
`guestPort`-off-macOS and Landlock §6 refusals are `invalidSpec`, raised inside the worker
after it is spawned, and stay as a second layer. Native macOS falls out because nothing in
the broker is tier-specific: only the cross-document rule's expected §6 spelling differs.

### S7. Lifecycle (proposed)

**The broker is owned by `ExecutionService`, not by a backend.** `ExecutionService::start`
is the chokepoint the direct-request path cannot route around, and the broker is
tier-agnostic (S6), so the service spawns and supervises it and the connections document
travels on `ExecutionRequest` beside the confinement manifest. A backend sees only a
confinement whose §6 paths are already bound.

Ordering inside `ExecutionService::start`, each step refusing the run on failure before the
next:

1. authorize the grant, including S3's all-or-nothing rule and the digest check;
2. parse the document (S2); check the tier (S6); run the cross-document rule (S2) and the
   endpoint checks, registering the run in the endpoint registry under its lock (S4);
3. spawn the broker and record the run as starting with its broker, **before** waiting
   for readiness;
4. send the document; the broker binds every endpoint, confines itself, and reports ready
   with the digest it ran (S5); no matching ready report within a bound (proposed: 5 s)
   fails the start (D7 rule 4);
5. compile the workload's confinement (§6 connect paths now resolve) and start the
   workload through the backend.

**Any failure after step 3** (a ready timeout or digest mismatch, a resolver error, a
worker spawn or compile failure, a `workspaceUnserved` refusal, the failed-cancel path)
kills the broker, unlinks its endpoints and removes the run from the registry before the
start returns. One exception: on the failed-cancel path the run is recorded `failed` while
its worker may still be alive, so its registry entry stays until `cleanup`'s cancel
succeeds; dropping it at the terminal state would let another run claim an endpoint the
old VMM can still dial. **Cancel during start** acts on a starting run: today `service.cancel` looks
only at `state.runs`, and a starting run lives in `state.starting` until `backend.start`
returns, so the service's cancel must also reach `starting` and kill the broker (and the
worker, if spawned). `ley-line-open-f963e6` names the same window one layer down, in
`KrunWorkerBackend::cancel`; the slice needs it at the service layer.

**Owned by the run, including when the supervisor dies** *(cloister; `ley-line-open-f963e6`)*.
The broker inherits the read end of a lifeline pipe whose only write end the supervising
process holds; end-of-file on it means the supervisor is gone, and the broker exits at once,
closing its listeners and every connection. This works on macOS, which has no
`PR_SET_PDEATHSIG`, and is the mechanism f963e6 asks for the krun worker; the two share one
implementation. Slice 1 does not ship before f963e6's parent-death fix, or ships that fix
with it: otherwise a broker outlives a killed supervisor exactly as a worker does today.

The lifeline holds only if no other process inherits its write end. macOS has no `pipe2`,
so close-on-exec is set after the pipe is created, and the daemon starts runs concurrently
and spawns workers with `posix_spawn`: another run's worker spawned in that window would
inherit run A's write end and keep A's broker alive after A's supervisor dies. Every spawn
the daemon makes (brokers and workers) therefore uses `POSIX_SPAWN_CLOEXEC_DEFAULT` with an
explicit list of inherited descriptors, so no descriptor crosses a spawn unless named. The
broker's control pipe follows the same rule.

**Teardown drains before it kills.** At every terminal state the supervisor asks the broker
over the control pipe to stop accepting and report a final record for every connection,
open ones included; it waits a bound (proposed: 1 s), then closes the lifeline, kills the
broker's process group and unlinks the endpoints. A connection open for the run's whole
life is thus in the receipt (S8).

### S8. The receipt (proposed)

Additive `RunReceipt` fields:

```capnp
connectionsDigest @15 :DigestRef               $Traits.optional;  # the document the broker attested in its ready report (S5)
connections       @16 :List(ConnectionOutcome) $Traits.optional;  # one per connection, in document order

struct ConnectionOutcome {
  name            @0 :Text;    # the connection's name
  target          @1 :Text;    # the literal dialed: "127.0.0.1:30080", or "[::1]:30080" for IPv6
  accepted        @2 :UInt64;  # connections accepted on this run's endpoint from this run's peer
  dialed          @3 :UInt64;  # of those, the ones whose upstream dial succeeded
  refused         @4 :UInt64;  # of those, the ones closed without an upstream (hold expired, dial error, cap)
  foreignPeers    @5 :UInt64;  # connections closed at accept because the peer was not this run's (S5); not in accepted
  complete        @6 :Bool;    # true when every accepted connection has a final record
  bytesToTarget   @7 :UInt64;
  bytesFromTarget @8 :UInt64;
}
```

*(cloister: each upstream dialled, the connection count, and refusals.)* The counts mean:
`accepted = dialed + refused` when `complete` is true; when it is false (the broker
crashed), the counts are lower bounds. Every count is attributed to this run because only
this run's peer is accepted (S4, S5). The broker reports each connection's record over its
control pipe when the connection ends, and a final record for each open connection at the
teardown drain (S7), so the receipt is assembled by the supervisor, not by the broker or
the workload. An open connection's byte counts at the drain are lower bounds: the broker
counts a chunk after its write returns, so the workload may already hold a chunk the
drain has not counted. A connection that ended before the drain has exact counts. After a broker crash the receipt carries every record reported before it,
`complete` is false, and the run's terminal state is `failed`. The receipt carries no
payload byte, no guest address and no peer pid.

**Receipt readers.** A receipt from a run without connections is byte-identical to today's,
in both the canonical Cap'n Proto form and the JSON carrier (null pointers are omitted).
A receipt from a run with connections carries two new keys, and the strict consumers
generated from the published asset (zod `.strict()`, JSON Schema
`additionalProperties: false`) built from v0.29.0 or earlier reject it. The receipt change
is therefore conditionally compatible: a reader must use the asset from the first release
that ships this slice before it reads receipts from runs with connections.

### S9. macOS and kind (deployment note only)

LLO dials a `host:port`. How the kind cluster's Service becomes reachable there is the
deployment's: on macOS, kind runs in Docker Desktop's VM, and a NodePort mapped with kind's
`extraPortMappings` (`listenAddress: 127.0.0.1`, a `hostPort` cloister picks) or a
`kubectl port-forward` that cloister runs appears on the Mac's `127.0.0.1:<hostPort>`. The
connections document carries that address. Nothing in LLO names a cluster, a Service, a
namespace or a kubeconfig, so LLO stays free of Kubernetes knowledge; if the cluster is not
up, the broker sees `ECONNREFUSED` and S5's bounded hold applies.

### S10. Threat model for slice 1

Assets: the authority to reach the declared targets and only them; the host's other
loopback services; the receipt's accuracy. There is no credential.

| threat | mechanism | rule |
|---|---|---|
| workload reaches an undeclared host port | dials another guest port, or the host directly | no guest listener exists for an undeclared port and hijack is off (Q7, I1/I2); the broker dials only its targets; its sandbox refuses other ports (S5) |
| workload steers the broker | an address in the stream | the broker parses nothing; the target is fixed per listener (S5) |
| a name resolves elsewhere at dial time | DNS answer changes | no names: IP literals only (S2) |
| endpoint squatting or a shared path | a process binds the path first; two runs share a directory | pre-existing path refused; parent must be a non-symlinked 0700 directory owned by the daemon's uid; bind before the workload's confinement compiles (S4) |
| another run reaches this run's broker | its §6 grant names this run's endpoint (the S0 shared-path join), its VMM running under the daemon's uid | the endpoint registry refuses either run at start, whichever registers second (S4); on the microVM tier the broker accepts only its own worker's pid (S5). Residual: the registry covers the runs of one `ExecutionService`; a second service under the same uid (an in-process embedder beside the daemon) is not covered on native macOS, which has no pid check |
| an unconfined local process uses the broker | connects to the endpoint | the parent is 0700 and the peer uid is checked; a same-uid unconfined process could already dial the loopback target itself, so it gains no reach (residual, accepted); on the microVM tier the pid check refuses it too |
| the workload replaces the broker's socket | unlinks the endpoint and binds its own (native macOS) | no §6 bind-capable grant on or containing an endpoint and no §2 write grant on its parent (S2) |
| upstream impersonation | another process holds the loopback port | not closed in slice 1: the edge is `policy` (S5); the receipt records the address, not the process |
| document tampered or swapped | config is ambient | `@16`/`@17` inside the signed grant; digest recomputed; non-canonical refused (S3) |
| an old daemon ignores the fields | unknown fields skipped | a verifying daemon refuses on the signature; every shipped daemon refuses the `credentialBrokerRefs` marker; clients check the capability first (S3) |
| the broker runs a different document | wrong bytes delivered | the ready report echoes the digest it ran; the supervisor compares it with `@16` (S5) |
| resource exhaustion | many connections, a stuck upstream | per-run connection cap; per-connection 10 s hold only on `ECONNREFUSED`; fixed buffers (S5) |
| broker compromise | a bug in the copy loop | no parser, no credential; profile denies the filesystem, exec, fork, the resolver and every port but the targets (S5) |
| broker outlives the run | supervisor killed; cancel during start; a failure after spawn | lifeline pipe with no inherited copies of its write end; the service's cancel reaches starting runs; every failure after step 3 kills the broker (S7) |
| descriptor passing | `SCM_RIGHTS` over the UNIX socket | received rights closed at once (`recvmsg(2)` with a control buffer; `read(2)` alone installs them on macOS), never forwarded (S5) |
| a declared connection silently not delivered | tier cannot express it; broker fails to bind | the run is refused by name (S2, S4, S6, S7) |

### S11. Tests that close it

Hermetic (no hypervisor). Hosted CI and the mutation job run on Linux, where slice 1 is
only refusals, so the tests marked *macOS* run in the macOS gates: local `task ci` on macOS
and the release dry-run's macOS job. Linux CI is not cited as evidence of macOS
enforcement.

- **Schema** (`leyline-schema-spec`, closes S2): the canonical vector and its pinned digest;
  one vector per parse refusal in S2's table, including a NUL, a control character and a
  non-ASCII byte in a path, a float and a string port, `localhost`, a non-loopback address
  and a non-canonical loopback spelling; accepted vectors with `127.0.0.2` and `::1`
  targets and their pinned digests (the schema already represents the range);
  a non-sorted input that canonicalizes to the canonical vector; an independent Python
  canonicalizer that reaches the pinned digest.
- **Grant vector** (closes S3): the pinned connections grant vector (S3, consumer
  surfaces): canonical bytes and signing-bytes digest, checked by `leyline-schema-spec`;
  a grant without the fields keeps its signing bytes (byte-equal to a pinned pre-field
  fixture).
- **Authorization** (closes S3): every combination of `@16`, `@17` and the marker (only
  all-or-nothing is accepted); a non-null empty `@16` or `@17`; a digest mismatch; a
  non-canonical `@17`; the marker plus a `broker://` entry is refused; a daemon that
  reads the fields but has no broker yet refuses a fields-plus-marker grant at start with
  `unsupportedBackend`, "this daemon has no connection broker", before the resolver runs,
  over binary and over JSON (amended 2026-11-11, `ley-line-open-521deb`: the wire half
  ships before the broker, and the old-daemon refusals are the next bullet's); a
  `127.0.0.2` or `::1` target is refused at start, "loopback address not delivered by this
  runtime", before anything is spawned (with the broker, `ley-line-open-52b5b3`).
- **Old daemon** (closes S3's default deny): the pinned grant vector fed once to the
  v0.29.0 release's `execution-daemon` in each verifier mode, recorded in the bead:
  `--trust-key` gives the `unauthenticated` signature refusal, `--allow-unverified-evidence`
  gives the `credentialBrokerRefs` text.
- **Capabilities** (closes S3): `capabilities_json` carries the connections entry once the
  daemon serves connections, and not before (a daemon never advertises what it cannot
  serve; until then it asserts the entry's absence).
- **Generated surfaces** (closes S3, S8): `execution-tools.json` shows the new fields and
  the rewritten `@13` description; a zod schema from the v0.29.0 asset rejects a
  connections receipt and the new asset accepts it; a receipt without connections is
  byte-identical JSON.
- **Cross-document and endpoint checks** (closes S2, S4): matching and mismatching pairs; a
  `connect-bind` directory grant over the channel directory; a §2 write grant on the
  parent; existing path, missing parent, symlinked parent, mode 0755, foreign owner.
- **Registry** (closes S4's run isolation): run B whose only §6 grant names run A's endpoint
  is refused, in both start orders (B before A registers, B after); A's receipt shows no
  connection from B.
- **Relay** (closes S5), a broker against a UNIX client and a TCP echo on loopback: bytes
  round-trip; half-close in each direction; upstream down then up within the hold; down
  past the hold gives end-of-file in no more than 10 s plus one backoff step; a client that
  hangs up ends the hold early; reset mid-stream; the cap; a second listener on another
  loopback port receives no connection; a passed `SCM_RIGHTS` descriptor never reaches the
  target and is closed; a ready report whose digest differs from `@16` refuses the start.
- **Profile** (closes S5; *macOS*): the whole relay suite runs again with the broker
  confined, as the positive control (accept and relay still work). Under the profile the
  broker gets `EPERM` connecting to an undeclared loopback port, opening any file,
  `execve` and `fork`, and `getaddrinfo` fails (connecting to mDNSResponder is pinned as
  reachable until `ley-line-open-fb5c4b`). The test pins
  the `mach-lookup` allow-set the spike fixes, so the set cannot grow without a test change.
- **Lifecycle** (closes S7): SIGKILL the supervising process and the broker is gone within
  a bound; `ExecutionService::cancel` during a stalled ready wait leaves no broker and no
  endpoint; a forced failure at step 5 leaves no broker and no endpoint; a broker killed
  mid-run fails the run; N concurrent starts leave each lifeline write end held only by
  the supervisor (checked with `lsof`).
- **Receipt** (closes S8): counts match a scripted sequence of accepted, dialed, refused and
  foreign-peer connections; a connection held open across workload exit appears with
  `complete` true; after a broker kill, `complete` is false.
- **Native macOS e2e** (closes S6 for that tier; *macOS*): a native workload dials the
  endpoint path and reaches the echo.

Hypervisor-gated (`LEYLINE_REQUIRE_HYPERVISOR=1`, macOS, in the release hypervisor gate):

- A host TCP echo on `127.0.0.1:E`; a document declaring one connection to `E`; a manifest
  with a §6 connect grant on the endpoint with `guestPort` G. A static guest binary dials
  `127.0.0.1:G`, writes bytes and reads them back. It then dials `127.0.0.1:G'`
  (undeclared) and gets `ECONNREFUSED`, and dials a non-loopback address of the host (its
  LAN address, and the gateway address a NAT backend would use) and fails (I1). A second
  host listener on `127.0.0.1:F` receives nothing. The receipt records one connection to
  `127.0.0.1:E`.
- The echo starts 2 s after the guest dials: the connection works. The echo never starts:
  the guest reads end-of-file within the hold, and the receipt counts one refusal.
- Two concurrent runs of one bundle with distinct channel directories both reach the echo;
  the same directory refuses the second by name.

### S11a. The loopback probe: the evidence for widening the delivered set

Slice 1 ships the probe that decides S-Q3's widening, so the widening is a gate flip and
not a new design. For each candidate address A in `127.0.0.1`, `127.0.0.2`,
`127.255.255.254` and `::1`, on each platform:

1. **Availability.** Bind a TCP listener on `A:P`. `EADDRNOTAVAIL` (for example `::1` in a
   container whose network namespace has no IPv6 loopback) records the row **unavailable**.
   Unavailable is never a pass: the runtime keeps refusing that address on that platform.
2. **Allowed.** In a child confined by the broker's profile with port P allowed, connect
   to `A:P`. Success records **scoped-allowed**; `EPERM` records **profile-blocks**, which
   means a document naming A would never dial under the profile.
3. **Port control.** With port Q ≠ P allowed instead, connect to `A:P`. It must fail with
   `EPERM`; success records **port-unscoped**, which fails the probe outright (the filter
   would be a wildcard).
4. **Address control.** With port P allowed for `127.0.0.1` only, connect to `A:P`. Success
   records the platform as **port-scoped only**: the profile cannot tell loopback addresses
   apart, and the broker's code alone holds the address (S2).

The test asserts a pinned expectation table per platform and prints the observed one. An
address joins the delivered set (S2) only when its row is scoped-allowed with the port
control passing on every platform that delivers slice 1 (today macOS). Changing the
delivered set changes that constant and the pinned table in one commit, and no schema,
vector or digest.

Expected findings, to be confirmed rather than assumed: on macOS, Seatbelt's
`remote tcp "localhost:N"` is port-scoped only, and whether it matches `127.0.0.2` and `::1`
is the open row. On Linux, Landlock `ConnectTcp` scopes by port only, so the address
control reads port-scoped only by construction; the Linux run matters for `::1`
availability and for the port control, because Linux CI is where a future Linux tier would
be proven.

**What CI needs.**

- **A macOS job on pull requests.** `ci.yml` runs only on `ubuntu-latest`; the one macOS
  runner today is in `release-dryrun.yml`. Slice 1 adds a `macos-latest` job to `ci.yml`
  that runs the probe and the other *macOS* tests of S11 (profile, native macOS end to end)
  through one task, `task runtime:broker-macos`, so local and CI run the same target. The
  probe extends `confinement_local_connect_probe.rs` (re-exec a child that confines itself
  and reports what it reached) and keeps its contract: the job sets
  `LEYLINE_REQUIRE_SANDBOX=1`, so a runner that cannot apply the profile fails by name
  instead of printing `SKIPPED (NOT a pass)` and going green.
- **The Linux probe in the existing job.** `ubuntu-latest` runs the probe on the host
  network, where `::1` is normally present, and a second run inside a container started
  with IPv6 disabled, which must record `::1` as unavailable. That proves the unavailable
  path refuses instead of passing.
- **Landlock ABI.** The Linux run records the kernel's Landlock ABI; below ABI 4 (no
  `ConnectTcp`) the port control is unexpressible and the row records that, by name.

### S12. Implementation outline (estimate)

- `rs/ll-core/schema-spec/connections/v1/` (README, schema, vectors, digest pins) and the
  `leyline-schema-spec` tests: bead `ley-line-open-e9d940`, scoped to S2.
- `execution.capnp`: `RunGrant @16 @17`, the `@13` doc text, `RunReceipt @15 @16`,
  `ConnectionOutcome`; `execution-tools.json` regenerated; the execution/v1 README,
  `run-grant.md`, the connections grant vector and `VECTORS.sha256`.
- `leyline-runtime`: authorization (S3); the transport capability entry; `ExecutionService`
  start ordering, the endpoint registry, cancel of starting runs and failure teardown (S4,
  S7); `ExecutionRequest` carrying the document; a broker module (preflight, relay, profile,
  control pipe, drain) and receipt assembly (S5, S8); a new LLO binary beside
  `leyline-native-worker`. The lifeline pipe is shared with `ley-line-open-f963e6`, which
  ships in the same release (S-Q5).
- The loopback probe and a `macos-latest` job in `ci.yml` running `task runtime:broker-macos`
  (S11a).
- **Every daemon spawn moves to `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT` (S7).**
  `std::process::Command` cannot set that flag, so the existing spawn paths in
  `native_backend.rs` and `libkrun/backend.rs` (`configure_process_group`) change too, not
  only the broker's. This is the widest part of the slice.
- **No libkrun patch.** Patch 0010's dial channel already joins guest loopback to the host
  path through vsock; the broker is the listener at that path.
- Cloister: emit the document, the digest and the marker; check the capability; read
  receipts with the new asset.

### S13. Slice-1 decisions (DECIDED 2026-11-11, owner)

The owner's answers, recorded as given:

- **S-Q1. DECIDED:** "accept as proposed. Accept the marker and the restated meaning of
  `credentialBrokerRefs`. Also accept the full design, including moving daemon spawns to
  posix_spawn."
- **S-Q2. DECIDED:** "a fixed cap of 256 simultaneous connections per run in Slice 1. A
  document field may come later as an additive change."
- **S-Q3. DECIDED:** "Feels like we should allow the whole probe now. Or at least plan for
  it and do just localhost for now. As we will need it for it to work in ci and stuff."
  Applied as: slice 1 enforces `127.0.0.1` only; the schema type and canonical form already
  represent `127.0.0.0/8` and `::1` (S2); slice 1 ships the loopback probe on macOS and on
  Linux CI (S11a); widening is a gate flip that a passing probe justifies. The receipt's
  IPv6 form is `[::1]:<port>` (S8).
- **S-Q4. DECIDED (delegated to this ADR; the owner leaned toward receipt fields):**
  per-connection outcomes are `RunReceipt` fields (S8). Reasons: the receipt is the
  digest-bound evidence a run produces, and an attester that includes the receipt's digest
  then covers the counts with no second object to fetch, store or retain; a separate
  detail document would need its own CAS storage and resolution, and a reader holding the
  receipt but not the document could not check it. The counts are a few fixed-size
  integers per declared connection, bounded by the document's length, so the receipt does
  not grow with traffic.
- **S-Q5. DECIDED:** "ship with the f963e6 fix in the same release: the broker and
  supervisor die together."

Out of scope for slice 1, recorded from review:

- **D4 and `@13`.** When credentials arrive, the "exactly the marker" rule becomes "the
  marker plus the broker entries D4 defines" (S3).
- **Signing bytes of a cleared signature (pre-existing).** `wire/run-grant.md` says a
  cleared `signature` canonicalizes to a null pointer that canonicalization drops, but
  `grant_signing_bytes` clears it with `init_signature()`, which leaves a non-null pointer
  to an empty struct, and `signature @14` is no longer the last pointer since `@15`. An
  independent signer that writes a null pointer would get different bytes. Not caused by
  this slice; it needs a test that pins one reading and a fix to whichever text is wrong.
- **Path-based checks.** The endpoint checks and bind go by path; binding through a
  directory descriptor would close the same-uid swap window (S4).

---

## Open questions — the decisions the user must make

- **Q1. Per-run or per-daemon broker?** D1 proposes per run. Per daemon is one process per
  trust domain with cross-run pooling, at the cost of co-locating every run's credentials.
  *Decided for Slice 1 (2026-11-11): per run (S1).*
- **Q2. Native Linux with credentials.** D5 refuses credential-bearing connections there.
  Alternatives: accept loopback TCP as a development-only tier with a named acknowledgment;
  give each run its own network namespace so its loopback is single-tenant; or wait for the
  Landlock stack to reach ABI 9 and deliver §6.
- **Q3. Authorize the document through a new `RunGrant` field (`connectionsDigest`)?** It is
  an additive execution/v1 wire change. The alternative is folding the connections into the
  confinement manifest, which mixes a broker's configuration into the workload's
  confinement digest. *Decided for Slice 1 (2026-11-11): yes, as `@16`/`@17` plus a
  `credentialBrokerRefs` marker so a shipped daemon refuses (S3; S-Q1).*
- **Q4. How the subject token reaches the broker.** Cloister obtains a notme token for the
  run's identity and delivers it with the grant (then a run longer than its lifetime needs
  a refresh path), or the broker authenticates to notme as the run directly (then the
  broker holds lane-2 credentials).
- **Q5. Static secrets for HTTP.** Relay through the credential-isolation/v1 vault proxy
  (the broker never sees the secret; two code paths) or hold a vault lease for every wire
  (one path; plaintext at the broker for HTTP too). Needs a vault operation cloister would
  define.
- **Q6. Pingora or hyper.** D6 recommends hyper, with the spike as the closing test.
- **Q7. The microVM guest relay. DECIDED (owner, 2026-11-10; record and invariants I1–I9
  on `ley-line-open-e2c5ce`; implemented by `ley-line-open-be3711`).** The rule is default
  deny wherever it is safe: an undeclared path must not exist, rather than be filtered.
  - LLO's guest init (libkrun patch 0010) presents each declared connection to an
    unmodified workload as TCP on the guest's loopback, in both directions. Dial: the
    workload connects to `127.0.0.1:N` and reaches exactly its declared peer. Serve: the
    workload listens on its declared port, and only the declared peer, through the §6
    socket, reaches it. A connection that arrives before the workload listens is held:
    the relay retries the workload's port on `ECONNREFUSED`, backing off from 5 ms to
    200 ms, for at most 10 s (`SERVE_HOLD_MS`, `ley-line-open-a915e3`), and closes the
    connection at that bound if nothing listens. Init binds every dial port before the workload starts, so loopback
    first-binder semantics confer nothing.
  - The connection-to-guest-address mapping is computed by the emitter and committed in
    the attested digest: confinement/v1 §6 `guestPort` (additive within v1). The worker and
    the guest derive nothing; the worker projects the document into the init's channel list,
    which a request cannot set.
  - VM-to-VM is two host-side channels (VM A's serve, VM B's dial) joined on the host. No
    guest-to-guest path exists.
  - UDP is denied in the guest: init refuses every datagram, raw and packet socket with a
    seccomp filter every guest process inherits.
  - TSI hijack stays off. It is an operator-only downgrade, and a `guestPort` under it is
    refused by name. Off macOS the presentation is refused by name until it runs live there.
  - The relay copies bytes only (no `SCM_RIGHTS`, no protocol awareness, no credential);
    D3 and D4 are out of its scope.
- **Q8. Endpoint allocation.** Cloister allocates socket paths (ADR-0069 §2), but the issuer
  must predict them for the digest (ADR-0036 equality contract), so a path cannot contain
  the run id. Either a deployment-configured broker root plus bundle and connection name
  (two concurrent runs of one bundle collide), or a run-unique component the issuer can
  predict. *Decided for Slice 1 (2026-11-11): the issuer allocates a per-run 0700
  channel directory and commits each path; LLO validates and chooses nothing (S4).*
- **Q9. Accept this ADR**, with or without amendments from the answers above.

---

## Consequences

- confinement/v1 gains its first consumer of both channel dimensions together: §6 for the
  broker sockets and §3 `connectLocal` for credential-free connections on native Linux.
- A new spec, `schema-spec/connections/v1`, with its own vectors and digest pins; cloister
  generates its emitter types from the schema instead of hand-mirroring them (ADR-0035 §6
  defect class).
- The runtime gains a second supervised process per run with connections, and its
  failed-start path covers it.
- LLO acquires GCP protocol code (STS exchange, IAM Credentials, the Cloud SQL connection
  protocol). That is mechanism. Which principal and which instance remain cloister's.
- Cloister ADR-0069 can cite this ADR as the named data contract once accepted, which is its
  remaining acceptance condition on the LLO side.

## Follow-up work (beads to file on acceptance)

Slice 1 (Accepted 2026-11-11), filed for v0.30.0, each blocking `ley-line-open-e2c5ce`:

- `ley-line-open-e9d940`: the `connections/v1` spec, schema, vectors and digests (S2).
- `ley-line-open-521deb`: the execution/v1 fields, the `@13` meaning, the capability
  entry, the grant vector and authorization (S3, S8).
- `ley-line-open-52b5b3`: the broker binary and module, its profile, preflight, the
  endpoint registry and the peer-pid check (S4, S5).
- `ley-line-open-5308b7`: `ExecutionService` start order, teardown, receipt assembly and
  the hypervisor end-to-end test (S2, S6, S7, S8).
- `ley-line-open-528525`: daemon spawns through `posix_spawn` (S7).
- `ley-line-open-52deb4`: the loopback probe and the macOS CI job (S11a).
- `ley-line-open-f963e6`: the parent-death fix, same release (S-Q5).

Earlier entries:

- `schema-spec/connections/v1`: README, schema, vectors, `CONNECTIONS_DIGESTS.blake3`, the
  schema-spec crate tests. Filed as `ley-line-open-e9d940`, blocked on this ADR (Q2–Q5, Q8).
- Broker spike (D6 exit criteria).
- LLO-internal broker confinement profile (D1).
- Guest-side loopback relay (Q7): done in `ley-line-open-be3711` (libkrun patches 0010 and
  0011, confinement/v1 §6 `guestPort`). Cloister's emitter allocates and emits `guestPort`
  (cloister bead filed with that change). The host-side join for VM-to-VM edges is not built.
- Runtime wiring: spawn before confinement, failed-start coverage, teardown.
- Cross-document check (D2) and the `connectionsDigest` field (Q3).
