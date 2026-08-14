# ADR-0038 — Proxied credentials enter at the host-side boundary, not the confined client

**Status:** Proposed (2026-08-12) — discussion target for `integration` before promotion to `main`
**Bead:** `ley-line-open-86d580`
**Related:**
- ADR-0035 (one confinement manifest; enforcement is attested)
- ADR-0037 (the named local proxy channel)
- `docs/superpowers/specs/2026-07-31-execution-v1-design.md` (LLO/Cloister ownership boundary)
- Cloister's harness-shim and CredentialVault design

## Context

The motivating case is Claude Code's `setup-token`, but the contract here is
for any confined client whose credential is held outside its execution
environment and whose ordinary request is sent through a host-side proxy. The
credential is **not located where the confined client is running**. It must not
be copied into the client environment merely to make the existing proxy path
work.

The relevant boundaries are easy to conflate:

| layer | authority | must see the token? |
|---|---|---|
| confined client process | ordinary request and proxy/base URL | **no** |
| host-side shim / credential ingress | approved host keystore reference and upstream request construction | transiently, in memory |
| product policy (Cloister in the motivating deployment) | which target may use which credential source and lease | reference/lease metadata, never plaintext by default |
| CredentialVault | encrypted storage, controlled retrieval, upstream authorization, and receipts | yes, inside its custody boundary |
| LLO/nono | generic host capability and confinement enforcement | no Claude- or Anthropic-specific knowledge |

`VAULT_KEK_SOURCE=keychain://...` does not solve this by itself. That setting
identifies the key-encryption key used by the vault; it is not a lookup for an
application credential. Likewise, a generic keychain resolver that
injects its result into a child process is correct for ordinary environment
credentials but wrong for this proxy-only use case.

There is a second unresolved fact that must remain explicit: whether a
`setup-token` is accepted as the exact upstream authorization form required by
the Anthropic passthrough. The security boundary can be designed now, but that
protocol fact needs an executable integration test before it is treated as a
shipped guarantee.

## Decision / proposal for review

Use a host-side credential-ingress path that is independent of the client
being proxied:

```text
confined client
    │ ordinary request + proxy/base URL only
    ▼
host-side shim
    │ resolves approved keychain reference before confinement
    │ or consumes an authenticated vault lease
    ▼
CredentialVault / upstream proxy
    │ attaches the upstream authorization for the declared credential type
    ▼
upstream service
    │
    └── Cloister receipt for the proxied call
```

The contract is:

1. **LLO/nono owns the generic primitive.** It may resolve an explicitly
   approved host-keystore reference before confinement and hand the result to
   a designated host-side component. LLO must not name Claude, Anthropic,
   `setup-token`, or Cloister vault policy.
2. **The product policy layer owns target policy.** In the motivating
   deployment this is Cloister: its harness target declares a subscription
   credential source (for example, a keychain reference or a vault lease),
   and Cloister decides when that source is allowed and binds it to the target
   and request policy. The generic ingress contract does not encode that
   product name or target.
3. **CredentialVault owns custody.** The host-side ingress stores the token
   through an authenticated, narrowly scoped vault-ingest/lease operation.
   The vault encrypts it at rest, retrieves it only for the matching upstream
   request, attaches the authorization, and emits the normal receipt. A
   receipt may attest the target, lease, and request; it must not contain the
   raw token.
4. **The confined process receives only the proxy contract.** It may receive
   the service's proxy/base-URL setting, but not a product-specific token
   environment variable, a keychain capability, an OAuth `Authorization`
   header, or a token-bearing file/argument/stdin stream.
5. **The existing API-key path remains separate.** API keys already handled by
   CredentialVault continue through their current custody path. A subscription
   token must not be smuggled into that path by pretending that
   the vault's KEK source is the user credential.
6. **Fail closed.** If the host-side resolver, authenticated vault ingress,
   lease binding, or upstream authorization validation is unavailable, the
   request is refused. The shim must not fall back to preserving an
   `Authorization` header supplied by the confined harness for this mode.

## Required evidence before implementation is called complete

The implementation should add an integration test that starts a confined
client-shaped workload with only the proxy URL and proves all of the following:

- the harness environment, argv, inherited descriptors, and granted files do
  not contain the token;
- the host-side shim or vault receives the approved keychain reference without
  exposing plaintext to the harness;
- the upstream test server receives the expected OAuth authorization;
- a request with no valid source, wrong target, expired lease, or unsupported
  token form is refused before egress;
- the receipt contains request/target/lease evidence but no credential bytes.

The test must exercise the actual proxy path, not only a unit test of a
keychain parser. For the motivating deployment, the `setup-token` acceptance
question is closed only when the upstream-compatible authorization test
passes; other credential types require their own upstream test vector.

## Ownership and non-goals

This ADR does **not** move product authentication policy into LLO, add a
client-specific schema to `nono`, or require LLO to access an OS keychain from
inside a Worker/DO. It records the seam so the generic LLO capability and the
product policy layer can evolve independently.

The transitional harness resolver may continue to support ordinary
environment-injection credentials. It must not be reused for subscription
tokens until the designated host-side sink and the evidence above exist.

## Open questions for Cloister review

1. Is the credential source a one-time authenticated vault ingest, a renewable
   lease, or a host shim that never persists the token? The choice changes
   recovery and revocation semantics.
2. What exact keychain item/reference format is approved, and how is its use
   authorized without granting the confined process keychain access?
3. For each supported product credential, what upstream authorization form
   does the proxy expect? Record the answer with a test vector, not prose;
   the motivating Claude `setup-token` case is only one such vector.
4. What receipt fields identify the credential lease while making it
   impossible to reconstruct the token?
