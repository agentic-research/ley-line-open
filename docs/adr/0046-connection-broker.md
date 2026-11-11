# ADR-0046 — The connection broker: the data plane for cloister's declared connections

**Status:** Proposed (2026-10-27; Slice 1 amendment proposed 2026-11-11). Acceptance is the
user's decision. See *Open questions*. Q7 is decided; every other answer in this document,
including all of *Slice 1*, is a proposal.
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
| confinement/v1 §6 `unixSocket.allow` | Shipped. Seatbelt per-path on native macOS; vsock↔UNIX mappings on the microVM tier (ADR-0036 O2); refused on native Linux (Landlock ≤ ABI 7 has no AF_UNIX right). |
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
  the peer that the host loopback cannot. The relay is new work (follow-up bead).
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

## Slice 1: credential-free relay (proposed for v0.30.0)

**Status of this section: proposed, not accepted.** The only owner decision it builds on is
Q7 (decided 2026-11-10, implemented by `ley-line-open-be3711`). Every S-numbered decision
below is a proposal. The constraints cloister supplied as the consumer (2026-11-11) are
marked *(cloister)*; where this section departs from one, it says so and why.

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
refused). `connectionsDigest` is BLAKE3-256 over those canonical bytes, rendered
`blake3-256:<hex>` as `DigestRef`. It names the connections document's bytes and nothing
else; it is not `confinementDigest`, a blob hash or any arena or store root.

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
| `target.tcp.host` | an IP literal in loopback: IPv4 dotted-quad in `127.0.0.0/8` with no leading zeros, or exactly `::1` | any name, including `localhost`; any non-loopback address, named "non-loopback target" |
| `target.tcp.port` | 1–65535 | 0 or out of range |
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
- **The OS profile can express it exactly.** Seatbelt scopes outbound TCP as
  `remote tcp "localhost:N"`, so the broker's own sandbox can hold it to the declared
  ports on loopback (S5). For an arbitrary host the profile could only scope by port,
  and the broker's code would be the sole enforcement.

**Cross-document rule, slice 1.** Checked at start, before the broker is spawned; a
mismatch refuses the run naming the connection (`invalidGrant`):

- every `endpoint.unix` appears exactly once in the confinement manifest's §6 `allow` as a
  connect grant on that exact path: on the microVM tier the object form
  `{"path", "mode": "connect", "guestPort"}`, on native macOS the bare string;
- no §6 grant on an endpoint path has `bind` or `connect-bind` (the workload never owns
  the broker's socket);
- a §6 connect grant that names no endpoint is not the broker's and is untouched (the
  VM-to-VM shared-path joins of S0 keep working).

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

**Signing bytes.** Both fields enter `signing_bytes = canonical(RunGrant with signature
cleared)` (`wire/run-grant.md`) with no change to that rule: they are pointer fields in the
signed struct, so a signature covers them. A grant that sets neither has null pointers in
those slots, which canonicalization truncates, so its signing bytes are byte-identical to
those of the same grant before the fields existed. Existing signatures stay valid.

**What an old daemon does, and why a new field alone is not enough.** A shipped daemon
skips an unknown pointer field in the binary form, and `capnp_json` skips an unknown key
(`transport.rs`, `refuse_unknown_limit_keys` documents the same gap for limits). An old
daemon handed only `@16`/`@17` would start the run without a broker, and the §6 connect
grant would then reach whatever binds the endpoint path. Default deny requires a refusal,
so the grant also carries a marker a shipped daemon already refuses:

- **`credentialBrokerRefs @13` = exactly `["cloister/connections/v1"]`.** Every daemon
  since execution/v1 first shipped (PR #317) refuses a non-empty `credentialBrokerRefs`
  with `unsupportedBackend`: "credential broker authority requires a broker integration"
  (`authorization.rs`). The field is "resolved credential brokers; never credential
  values", and the connection broker is that broker. The entry names the contract, not a
  credential, and is inside the signing bytes.
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
The old daemon's own refusal is the fixed `credentialBrokerRefs` text above; it cannot name
a version it predates, so the version is the client's to state.

**Consumer surfaces.** The schema change regenerates every checked-in consumer of
`execution.capnp` (Rust bindings, `execution.tools.json`, the Go and TypeScript fixtures,
compatibility metadata). Cloister's emitter must reproduce `connectionsDigest` byte for
byte from the `connections/v1` vectors (`ley-line-open-e9d940`) before it sends one
*(cloister: conditional on those vectors)*.

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
| the parent is owned by the daemon's effective uid and its mode is exactly 0700 | `invalidGrant`: "endpoint parent not private" |
| the full path fits this platform's `sun_path` | `invalidGrant`: "endpoint path too long" (the 103-byte parse rule makes this unreachable on the supported platforms; it stays as the platform check) |
| no two connections share a path | refused at parse (S2) |

The broker binds every endpoint before the run reports ready, and before the workload's
confinement compiles, which §6's ordering rule requires of a connect grant *(cloister:
`up` waits for the socket to exist)*. Slice 1 allocates nothing on the issuer's behalf, so
`StartOutput` gains no field. The broker unlinks its sockets when it exits and the
supervisor unlinks any it left; the channel directory is the issuer's to remove.

Checking ownership and mode closes a gap the issuer's convention leaves to discipline: a
0700 directory owned by another uid, or reached through a symlink, would let a process
outside the run choose what the path resolves to between the issuer's check and the bind.

### S5. The relay: allow-list enforcement and failure behaviour (proposed)

**The broker dials exactly the committed target and nothing else.**

- Each listener is bound to one connection; the target is fixed when the broker starts
  from the authorized document, as a `SocketAddr` built from the literal. The broker reads
  no address from the stream, parses no protocol and follows nothing, so the workload has
  no way to steer it.
- One upstream dial per accepted connection; no pooling, no reuse across connections, no
  dial before a connection arrives.
- **Bind, then confine.** The broker binds every endpoint, then applies its LLO-internal
  nono profile to itself, then reports ready. The profile denies the filesystem entirely
  (accepting on an already-bound listener needs no path right), allows outbound TCP to
  `localhost:<port>` for each declared port and nothing else, and allows no inbound TCP,
  no listen, no exec. Two layers enforce the allow-list: the broker's code dials one
  address per listener, and its sandbox refuses any other port.
- The peer is checked on accept: `getpeereid` must give the daemon's effective uid (on the
  microVM tier the peer is the run's VMM). Another uid is closed without a dial and counted
  as a refusal.

**Failure modes.** A guest connection is accepted by the guest init relay before it reaches
the host (patch 0010), so a guest client never sees `ECONNREFUSED` from a declared port; it
sees bytes or end-of-file *(cloister: refused or EOF)*.

| event | broker behaviour | the guest sees |
|---|---|---|
| upstream `ECONNREFUSED` (nothing listening yet, e.g. kind still starting) | holds the accepted connection and retries, backing off from 5 ms to 200 ms, for at most 10 s, ending early when the guest side hangs up; the same bound as a serve channel's hold (patch 0010, #554) *(cloister)* | the connection works once the upstream is up within 10 s; otherwise end-of-file without a byte |
| any other dial error (`ECONNRESET`, `ETIMEDOUT`, `EHOSTUNREACH`, profile `EPERM`) | closes at once, no retry | end-of-file |
| upstream resets mid-stream | closes both directions | end-of-file (a UNIX stream peer on macOS observes no reset, and the vsock hop carries none) |
| half-close: one side ends its writes | `shutdown(SHUT_WR)` toward the other side; the other direction keeps copying until it ends | the same half-close |
| more concurrent connections than the run's cap (proposed: 256 per run) | closes the new connection without a dial; counted as a refusal | end-of-file |
| broker exits or crashes during the run | the supervisor fails the run (D7 rule 5): cancels the workload, terminal state `failed`, receipt written from what the supervisor holds | end-of-file on open connections; new dials end at once (nothing listens at the path) |

There is no unbounded retry anywhere: the 10 s hold is per connection and only for
`ECONNREFUSED`. Each direction has a fixed buffer (64 KiB) and is copied with `read(2)` and
`write(2)` only, so no ancillary data is received or sent (D7, `SCM_RIGHTS`).

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

Refusal is `unsupportedBackend`, naming the tier and the connection, before anything is
spawned (D7 rule 3). Native macOS falls out because nothing in the broker is tier-specific:
only the cross-document rule's expected §6 spelling differs.

### S7. Lifecycle (proposed)

Ordering inside `start`, each step refusing the run on failure before the next:

1. authorize the grant, including S3's all-or-nothing rule and the digest check;
2. parse the document (S2), run the cross-document rule (S2) and the endpoint checks (S4);
3. spawn the broker and record it in the run's state **before** waiting for readiness, so
   `cancel` in that window kills it (the second defect of `ley-line-open-f963e6`);
4. the broker binds every endpoint, confines itself, and reports ready over its control
   pipe; no ready report within a bound (proposed: 5 s) kills it, unlinks the endpoints and
   fails the start on the existing failed-start path (D7 rule 4);
5. compile the workload's confinement (§6 connect paths now resolve) and start the
   workload.

**Owned by the run, including when the supervisor dies** *(cloister; `ley-line-open-f963e6`)*.
The broker inherits the read end of a lifeline pipe whose only write end the supervising
process holds; end-of-file on it means the supervisor is gone, and the broker exits at once,
closing its listeners and every connection. This works on macOS, which has no
`PR_SET_PDEATHSIG`, and is the mechanism f963e6 asks for the krun worker; the two share one
implementation. Slice 1 does not ship before f963e6's parent-death fix, or ships that fix
with it: otherwise a broker outlives a killed supervisor exactly as a worker does today.
At every terminal state the supervisor closes the lifeline, kills the broker's process
group and unlinks the endpoints.

### S8. The receipt (proposed)

Additive `RunReceipt` fields:

```capnp
connectionsDigest @15 :DigestRef               $Traits.optional;  # the document the broker ran
connections       @16 :List(ConnectionOutcome) $Traits.optional;  # one per connection, in document order

struct ConnectionOutcome {
  name            @0 :Text;    # the connection's name
  target          @1 :Text;    # the literal dialled, "127.0.0.1:30080"
  accepted        @2 :UInt64;  # connections accepted on the endpoint
  dialled         @3 :UInt64;  # upstream dials that succeeded
  refused         @4 :UInt64;  # accepted, closed without an upstream (refused, hold expired, dial error, cap, foreign peer)
  bytesToTarget   @5 :UInt64;
  bytesFromTarget @6 :UInt64;
}
```

*(cloister: each upstream dialled, the connection count, and refusals.)* The broker reports
each connection's record to the supervisor over its control pipe when the connection ends,
so the receipt is assembled by the supervisor, not by the broker or the workload. After a
broker crash, the receipt carries every record reported before it; connections open at the
crash are absent, and the run's terminal state is `failed`. The receipt carries no payload
byte, no guest address and no peer pid. A consumer that predates the fields reads a
receipt without them; absence is "no connections", which is true of every run such a
daemon produced.

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
| a local process uses the broker | connects to the endpoint | the parent is 0700 and the peer uid is checked; a same-uid process could already dial the loopback target itself, so it gains no reach (residual, accepted) |
| upstream impersonation | another process holds the loopback port | not closed in slice 1: the edge is `policy` (S5); the receipt records the address, not the process |
| document tampered or swapped | config is ambient | `@16`/`@17` inside the signed grant; digest recomputed; non-canonical refused (S3) |
| an old daemon ignores the fields | unknown fields skipped | the `credentialBrokerRefs` marker is refused by every shipped daemon; clients check the capability first (S3) |
| resource exhaustion | many connections, a stuck upstream | per-run connection cap; per-connection 10 s hold only on `ECONNREFUSED`; fixed buffers (S5) |
| broker compromise | a bug in the copy loop | no parser, no credential; profile denies the filesystem and every port but the targets (S5) |
| broker outlives the run | supervisor killed; cancel before ready | lifeline pipe; recorded before readiness wait (S7) |
| descriptor passing | `SCM_RIGHTS` over the UNIX socket | `read(2)`/`write(2)` only (S5) |
| a declared connection silently not delivered | tier cannot express it; broker fails to bind | the run is refused by name (S2, S4, S6, S7) |

### S11. Tests that close it

Hermetic (no hypervisor; in `task ci`):

- **Schema** (`leyline-schema-spec`, closes S2): the canonical vector and its pinned digest;
  one vector per parse refusal in S2's table; a non-sorted input that canonicalizes to the
  canonical vector.
- **Authorization** (closes S3): every combination of `@16`, `@17` and the marker (only
  all-or-nothing is accepted); a digest mismatch; a non-canonical `@17`; a signed grant
  without the fields keeps its signing bytes (byte-equal to a pinned pre-field fixture).
- **Old daemon** (closes S3's default deny): a pinned fixture grant with the fields and the
  marker, fed to the v0.29.0 release's `execution-daemon` once and recorded in the bead,
  refuses with the `credentialBrokerRefs` text; in CI, the current daemon built without a
  broker refuses the same fixture the same way.
- **Capabilities** (closes S3): `capabilities_json` carries the connections entry.
- **Cross-document and endpoint checks** (closes S2, S4): matching and mismatching pairs;
  existing path, missing parent, symlinked parent, mode 0755, foreign owner.
- **Relay** (closes S5), a broker against a UNIX client and a TCP echo on loopback: bytes
  round-trip; half-close in each direction; upstream down then up within the hold; down
  past the hold gives end-of-file in no more than 10 s plus one backoff step; a client that
  hangs up ends the hold early; reset mid-stream; the cap; a second listener on another
  loopback port receives no connection; a passed `SCM_RIGHTS` descriptor never reaches the
  target and is closed.
- **Profile** (closes S5, macOS): the confined broker gets `EPERM` connecting to an
  undeclared loopback port and opening any file.
- **Lifecycle** (closes S7): SIGKILL the supervising process and the broker is gone within
  a bound; cancel between spawn and ready leaves no broker and no endpoint; a broker killed
  mid-run fails the run.
- **Receipt** (closes S8): counts match a scripted sequence of accepted, dialled and refused
  connections.
- **Native macOS e2e** (closes S6 for that tier): a native workload dials the endpoint path
  and reaches the echo.

Hypervisor-gated (`LEYLINE_REQUIRE_HYPERVISOR=1`, macOS, in the release hypervisor gate):

- A host TCP echo on `127.0.0.1:E`; a document declaring one connection to `E`; a manifest
  with a §6 connect grant on the endpoint with `guestPort` G. A static guest binary dials
  `127.0.0.1:G`, writes bytes and reads them back. It then dials `127.0.0.1:G'` (undeclared)
  and gets `ECONNREFUSED`, and dials the host's echo directly and fails (I1). A second host
  listener on `127.0.0.1:F` receives nothing. The receipt records one connection to
  `127.0.0.1:E`.
- The echo starts 2 s after the guest dials: the connection works. The echo never starts:
  the guest reads end-of-file within the hold, and the receipt counts one refusal.
- Two concurrent runs of one bundle with distinct channel directories both reach the echo;
  the same directory refuses the second by name.

### S12. Implementation outline (estimate)

- `rs/ll-core/schema-spec/connections/v1/` (README, schema, vectors, digest pins) and the
  `leyline-schema-spec` tests: bead `ley-line-open-e9d940`, scoped to S2.
- `execution.capnp`: `RunGrant @16 @17`, `RunReceipt @15 @16`, `ConnectionOutcome`;
  regenerated bindings, `execution.tools.json`, consumer fixtures, compatibility metadata;
  `run-grant.md` and the execution/v1 README.
- `leyline-runtime`: authorization (S3), the transport capability entry, a broker module
  (preflight, relay, profile, control pipe), the `start` ordering and receipt assembly
  (S7, S8), the native and libkrun backends' spawn order; a new LLO binary beside
  `leyline-native-worker`. The lifeline pipe is shared with `ley-line-open-f963e6`.
- **No libkrun patch.** Patch 0010's dial channel already joins guest loopback to the host
  path through vsock; the broker is the listener at that path.
- Cloister: emit the document, the digest and the marker; check the capability.

### S13. Slice-1 questions for the owner

- **S-Q1.** Accept the `credentialBrokerRefs` marker as the old-daemon refusal (S3), or
  accept that only clients that check capabilities are protected.
- **S-Q2.** The per-run connection cap: a fixed 256, or a document field.
- **S-Q3.** `::1` as a target, or IPv4 loopback only.
- **S-Q4.** Per-connection outcomes as receipt fields (S8), or a content-addressed detail
  document the receipt names by digest.

---

## Open questions — the decisions the user must make

- **Q1. Per-run or per-daemon broker?** D1 proposes per run. Per daemon is one process per
  trust domain with cross-run pooling, at the cost of co-locating every run's credentials.
  *Proposed answer: per run (Slice 1, S1).*
- **Q2. Native Linux with credentials.** D5 refuses credential-bearing connections there.
  Alternatives: accept loopback TCP as a development-only tier with a named acknowledgment;
  give each run its own network namespace so its loopback is single-tenant; or wait for the
  Landlock stack to reach ABI 9 and deliver §6.
- **Q3. Authorize the document through a new `RunGrant` field (`connectionsDigest`)?** It is
  an additive execution/v1 wire change. The alternative is folding the connections into the
  confinement manifest, which mixes a broker's configuration into the workload's
  confinement digest. *Proposed answer: yes, as `@16`/`@17` plus a `credentialBrokerRefs`
  marker so a shipped daemon refuses (Slice 1, S3; S-Q1).*
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
    socket, reaches it. Init binds every dial port before the workload starts, so loopback
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
  predict. *Proposed answer: the issuer allocates a per-run 0700 channel directory and
  commits each path; LLO validates and chooses nothing (Slice 1, S4).*
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

- `schema-spec/connections/v1`: README, schema, vectors, `CONNECTIONS_DIGESTS.blake3`, the
  schema-spec crate tests. Filed as `ley-line-open-e9d940`, blocked on this ADR (Q2–Q5, Q8).
- Broker spike (D6 exit criteria).
- LLO-internal broker confinement profile (D1).
- Guest-side loopback relay (Q7): done in `ley-line-open-be3711` (libkrun patches 0010 and
  0011, confinement/v1 §6 `guestPort`). Cloister's emitter allocates and emits `guestPort`
  (cloister bead filed with that change). The host-side join for VM-to-VM edges is not built.
- Runtime wiring: spawn before confinement, failed-start coverage, teardown.
- Cross-document check (D2) and the `connectionsDigest` field (Q3).
