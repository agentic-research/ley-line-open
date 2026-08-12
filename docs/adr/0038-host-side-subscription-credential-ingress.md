# ADR-0038 — Subscription credentials enter at the host-side proxy, not the confined harness

**Status:** Proposed (2026-08-12) — discussion target for `integration` before promotion to `main`
**Bead:** `ley-line-open-86d580`
**Related:**
- ADR-0035 (one confinement manifest; enforcement is attested)
- ADR-0037 (the named local proxy channel)
- `docs/superpowers/specs/2026-07-31-execution-v1-design.md` (LLO/Cloister ownership boundary)
- Cloister's harness-shim and CredentialVault design

## Context

Claude Code's `setup-token` is a subscription credential. In the intended
deployment, the token is **not located where the confined Claude process is
running**: a host-side shim proxies Claude's ordinary request to Anthropic.
The token must therefore not be copied into the harness environment merely to
make the existing proxy path work.

The relevant boundaries are easy to conflate:

| layer | authority | must see the token? |
|---|---|---|
| confined Claude process | ordinary client request and proxy/base URL | **no** |
| host-side shim / credential ingress | approved host keystore reference and upstream request construction | transiently, in memory |
| Cloister policy | which Claude target may use which credential source and lease | reference/lease metadata, never plaintext by default |
| CredentialVault | encrypted storage, controlled retrieval, upstream authorization, and receipts | yes, inside its custody boundary |
| LLO/nono | generic host capability and confinement enforcement | no Claude- or Anthropic-specific knowledge |

`VAULT_KEK_SOURCE=keychain://...` does not solve this by itself. That setting
identifies the key-encryption key used by the vault; it is not a lookup for a
user's Claude subscription token. Likewise, a generic keychain resolver that
injects its result into a child process is correct for ordinary environment
credentials but wrong for this proxy-only use case.

There is a second unresolved fact that must remain explicit: whether a
`setup-token` is accepted as the exact upstream authorization form required by
the Anthropic passthrough. The security boundary can be designed now, but that
protocol fact needs an executable integration test before it is treated as a
shipped guarantee.

## Decision / proposal for review

Use a host-side credential-ingress path:

```text
Claude Code (confined)
    │ ordinary request + proxy/base URL only
    ▼
host-side shim
    │ resolves approved keychain reference before confinement
    │ or consumes an authenticated vault lease
    ▼
CredentialVault / Anthropic proxy
    │ attaches upstream OAuth authorization
    ▼
Anthropic
    │
    └── Cloister receipt for the proxied call
```

The contract is:

1. **LLO/nono owns the generic primitive.** It may resolve an explicitly
   approved host-keystore reference before confinement and hand the result to
   a designated host-side component. LLO must not name Claude, Anthropic,
   `setup-token`, or Cloister vault policy.
2. **Cloister owns product policy.** A Claude harness target declares a
   subscription credential source (for example, a keychain reference or a
   vault lease). Cloister decides when that source is allowed and binds it to
   the target and request policy.
3. **CredentialVault owns custody.** The host-side ingress stores the token
   through an authenticated, narrowly scoped vault-ingest/lease operation.
   The vault encrypts it at rest, retrieves it only for the matching upstream
   request, attaches the authorization, and emits the normal receipt. A
   receipt may attest the target, lease, and request; it must not contain the
   raw token.
4. **The confined process receives only the proxy contract.** It may receive
   `ANTHROPIC_BASE_URL` (or the equivalent proxy setting), but not
   `CLAUDE_CODE_OAUTH_TOKEN`, a keychain capability, an OAuth `Authorization`
   header, or a token-bearing file/argument/stdin stream.
5. **The existing API-key path remains separate.** API keys already handled by
   CredentialVault continue through their current custody path. A Claude
   subscription token must not be smuggled into that path by pretending that
   the vault's KEK source is the user credential.
6. **Fail closed.** If the host-side resolver, authenticated vault ingress,
   lease binding, or upstream authorization validation is unavailable, the
   request is refused. The shim must not fall back to preserving an
   `Authorization` header supplied by the confined harness for this mode.

## Required evidence before implementation is called complete

The implementation should add an integration test that starts a confined
Claude-shaped client with only the proxy URL and proves all of the following:

- the harness environment, argv, inherited descriptors, and granted files do
  not contain the token;
- the host-side shim or vault receives the approved keychain reference without
  exposing plaintext to the harness;
- the upstream test server receives the expected OAuth authorization;
- a request with no valid source, wrong target, expired lease, or unsupported
  token form is refused before egress;
- the receipt contains request/target/lease evidence but no credential bytes.

The test must exercise the actual proxy path, not only a unit test of a
keychain parser. The `setup-token` acceptance question is closed only when the
upstream-compatible authorization test passes.

## Ownership and non-goals

This ADR does **not** move Claude authentication policy into LLO, add a
Claude-specific schema to `nono`, or require LLO to access an OS keychain from
inside a Worker/DO. It records the seam so the generic LLO capability and the
Cloister product policy can evolve independently.

The transitional harness resolver may continue to support ordinary
environment-injection credentials. It must not be reused for subscription
tokens until the designated host-side sink and the evidence above exist.

## Open questions for Cloister review

1. Is the credential source a one-time authenticated vault ingest, a renewable
   lease, or a host shim that never persists the token? The choice changes
   recovery and revocation semantics.
2. What exact keychain item/reference format is approved, and how is its use
   authorized without granting the confined process keychain access?
3. Does `setup-token` produce the same upstream authorization form as the
   Anthropic proxy expects? Record the answer with a test vector, not prose.
4. What receipt fields identify the credential lease while making it
   impossible to reconstruct the token?

