# ADR-0046 — The connection broker: the data plane for cloister's declared connections

**Status:** Proposed (2026-10-27). Acceptance is the user's decision. See *Open questions*.
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

## Open questions — the decisions the user must make

- **Q1. Per-run or per-daemon broker?** D1 proposes per run. Per daemon is one process per
  trust domain with cross-run pooling, at the cost of co-locating every run's credentials.
- **Q2. Native Linux with credentials.** D5 refuses credential-bearing connections there.
  Alternatives: accept loopback TCP as a development-only tier with a named acknowledgment;
  give each run its own network namespace so its loopback is single-tenant; or wait for the
  Landlock stack to reach ABI 9 and deliver §6.
- **Q3. Authorize the document through a new `RunGrant` field (`connectionsDigest`)?** It is
  an additive execution/v1 wire change. The alternative is folding the connections into the
  confinement manifest, which mixes a broker's configuration into the workload's
  confinement digest.
- **Q4. How the subject token reaches the broker.** Cloister obtains a notme token for the
  run's identity and delivers it with the grant (then a run longer than its lifetime needs
  a refresh path), or the broker authenticates to notme as the run directly (then the
  broker holds lane-2 credentials).
- **Q5. Static secrets for HTTP.** Relay through the credential-isolation/v1 vault proxy
  (the broker never sees the secret; two code paths) or hold a vault lease for every wire
  (one path; plaintext at the broker for HTTP too). Needs a vault operation cloister would
  define.
- **Q6. Pingora or hyper.** D6 recommends hyper, with the spike as the closing test.
- **Q7. The microVM guest relay.** Accept that LLO's guest init grows a loopback→vsock relay
  so unmodified TCP clients work on the microVM tier, or require workloads to dial sockets.
- **Q8. Endpoint allocation.** Cloister allocates socket paths (ADR-0069 §2), but the issuer
  must predict them for the digest (ADR-0036 equality contract), so a path cannot contain
  the run id. Either a deployment-configured broker root plus bundle and connection name
  (two concurrent runs of one bundle collide), or a run-unique component the issuer can
  predict.
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
- Guest-side loopback→vsock relay (Q7).
- Runtime wiring: spawn before confinement, failed-start coverage, teardown.
- Cross-document check (D2) and the `connectionsDigest` field (Q3).
