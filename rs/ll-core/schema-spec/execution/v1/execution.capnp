@0xc9778e78d9783408;

using Traits = import "../../_traits.capnp";

# execution/v1 is a substrate contract. Product policy is resolved into a
# RunGrant before this boundary; backend implementations remain private.

enum BackendClass $Traits.doc("Isolation class required by resolved policy.") {
  native  @0;
  microVm @1;
}

enum RunState $Traits.doc("Append-only execution lifecycle state.") {
  accepted     @0;
  provisioning @1;
  ready        @2;
  running      @3;
  succeeded    @4;
  failed       @5;
  cancelled    @6;
  cleaning     @7;
  cleaned      @8;
}

enum ErrorCode $Traits.doc("Stable error classification shared by every transport.") {
  invalidSpec            @0;
  invalidGrant           @1;
  unauthenticated        @2;
  unauthorized           @3;
  identityPolicyMismatch @4;
  unsupportedBackend     @5;
  notProvisioned         @6;
  resourceConflict       @7;
  resourceExhausted      @8;
  backendFailed          @9;
  cancelled              @10;
  internal               @11;
}

enum WorkspaceOperation $Traits.doc("Operation authorized on one logical Graph workspace.") {
  read   @0;
  list   @1;
  query  @2;
  mutate @3;
  commit @4;
}

enum CancellationMode $Traits.doc("Behavior requested when a caller cancels or disconnects.") {
  explicitOnly       @0;
  cancelOnDisconnect @1;
}

struct DigestRef $Traits.doc("Algorithm-tagged content digest.") {
  algorithm @0 :Text $Traits.doc("Digest algorithm, for example blake3-256 or sha256.");
  value     @1 :Text $Traits.doc("Lowercase digest bytes encoded as hexadecimal text.");
}

struct EvidenceRef $Traits.doc("Content-addressed identity, provenance, or enforcement evidence.") {
  mediaType @0 :Text      $Traits.doc("Versioned media type identifying the external evidence contract.");
  digest    @1 :DigestRef $Traits.doc("Digest of the canonical evidence bytes.");
}

struct ArtifactRef $Traits.doc("Immutable content-addressed executable or data artifact.") {
  digest    @0 :DigestRef $Traits.doc("Artifact content digest.");
  mediaType @1 :Text      $Traits.doc("Artifact media type.");
}

struct KeyValue $Traits.doc("Public non-secret environment entry.") {
  key   @0 :Text $Traits.doc("Environment variable name.");
  value @1 :Text $Traits.doc("Public value. Secret material is forbidden here.");
}

struct SecretHandle $Traits.doc("Opaque reference to a credential broker entry.") {
  name      @0 :Text $Traits.doc("Name exposed to the workload.");
  brokerRef @1 :Text $Traits.doc("Opaque broker reference; never the credential value.");
}

struct ResourceLimits $Traits.doc("Resolved or requested resource ceilings.") {
  wallTimeMs   @0 :UInt64 $Traits.doc("Maximum wall-clock runtime in milliseconds; zero means policy default.");
  memoryBytes  @1 :UInt64 $Traits.doc("Maximum memory in bytes; zero means policy default.");
  cpuMillis    @2 :UInt64 $Traits.doc("Maximum cumulative CPU time in milliseconds; zero means policy default.");
  outputBytes  @3 :UInt64 $Traits.doc("Maximum collected output bytes; zero means policy default.");
}

struct WorkspaceIntent $Traits.doc("Requested logical Graph workspace; carries no host path authority.") {
  name      @0 :Text      $Traits.doc("Name by which the workload addresses this workspace.");
  graphRoot @1 :DigestRef $Traits.doc("Immutable input Graph root.");
}

struct WorkspaceGrant $Traits.doc("Resolved authority over one logical Graph workspace.") {
  name               @0 :Text                     $Traits.doc("RunSpec workspace name this grant resolves.");
  graphRoot          @1 :DigestRef                $Traits.doc("Authorized input Graph root.");
  expectedGeneration @2 :UInt64                   $Traits.doc("Optimistic-concurrency generation for mutation.");
  operations         @3 :List(WorkspaceOperation) $Traits.doc("Exact operations permitted on this workspace.");
}

struct CapabilityGrant $Traits.doc("Verified authorization grant mapped to a versioned interface.") {
  grant     @0 :Text $Traits.doc("Lane-1 Signet capability URN.");
  interface @1 :Text $Traits.doc("Lane-3 interface identifier, for example cloister/execution/v1.");
}

struct OutputDeclaration $Traits.doc("Named content-addressed result expected from a run.") {
  name      @0 :Text $Traits.doc("Stable result name.");
  mediaType @1 :Text $Traits.doc("Expected result media type.");
}

struct RunSpec $Traits.doc("Content-addressed execution intent. It is not authority.") {
  schemaVersion         @0  :Text                    $Traits.doc("Contract identifier; must be cloister/execution/v1.");
  executable            @1  :ArtifactRef             $Traits.doc("Immutable executable or guest artifact.");
  arguments             @2  :List(Text)              $Traits.doc("Argument vector excluding argv[0].");
  workspaceInputs       @3  :List(WorkspaceIntent)   $Traits.doc("Logical content-addressed workspaces requested by name.");
  publicEnvironment     @4  :List(KeyValue)          $Traits.doc("Non-secret environment values.");
  secretHandles         @5  :List(SecretHandle)      $Traits.doc("Opaque secret broker handles; never secret values.");
  requestedInterfaces   @6  :List(Text)              $Traits.doc("Lane-3 interfaces requested by the workload.");
  requestedLimits       @7  :ResourceLimits          $Traits.doc("Requested ceilings; the grant may only narrow them.");
  outputs               @8  :List(OutputDeclaration) $Traits.doc("Declared content-addressed outputs.");
  cancellationMode      @9  :CancellationMode        $Traits.doc("Requested cancellation behavior.");
  compatibilityRuntime  @10 :ArtifactRef             $Traits.doc("Optional guest runtime artifact; an empty digest means none.") $Traits.optional;
}

struct GrantSignature $Traits.doc("Detached issuer signature over a RunGrant. See wire/run-grant.md for the covered bytes.") {
  algorithm @0 :Text $Traits.doc("Signature algorithm; ed25519 is the only value this contract defines.");
  keyId     @1 :Text $Traits.doc("Unauthenticated issuer key hint. A verifier checks every trusted key regardless and never selects one by this.");
  value     @2 :Data $Traits.doc("Raw signature bytes; 64 for ed25519.");
}

struct RunGrant $Traits.doc("Authenticated, resolved execution authority bound to one RunSpec digest.") {
  grantId                    @0  :Text                  $Traits.doc("Issuer-unique grant identifier.");
  issuerEvidence             @1  :EvidenceRef           $Traits.doc("Verified issuer/signature evidence. Must be bound to this run — see the evidence-binding rule in README.md.");
  expiresAtUnixMs            @2  :UInt64                $Traits.doc("Absolute expiry in Unix milliseconds.");
  replayKey                  @3  :Text                  $Traits.doc("Idempotency and replay-protection key.");
  runSpecDigest              @4  :DigestRef             $Traits.doc("Digest of the authorized RunSpec.");
  workloadIdentityEvidence   @5  :EvidenceRef           $Traits.doc("Verified Interlace/WIMSE workload identity evidence. Must be bound to this run — see the evidence-binding rule in README.md.");
  actorProvenanceEvidence    @6  :EvidenceRef           $Traits.doc("Verified delegated actor/provenance evidence, such as a Signet bridge certificate. Must be bound to this run — see the evidence-binding rule in README.md.");
  capabilities               @7  :List(CapabilityGrant) $Traits.doc("Exact verified grants and interface mappings.");
  confinementDigest          @8  :DigestRef             $Traits.doc("confinement/v1 digest that enforcement must match.");
  backendClass               @9  :BackendClass          $Traits.doc("Required isolation class; callers cannot weaken it.");
  limits                     @10 :ResourceLimits        $Traits.doc("Resolved ceilings.");
  workspaces                 @11 :List(WorkspaceGrant)  $Traits.doc("Resolved Graph authority.");
  allowedEgress              @12 :List(Text)            $Traits.doc("Resolved egress endpoints or capability references.");
  credentialBrokerRefs       @13 :List(Text)            $Traits.doc("Resolved credential brokers; never credential values.");
  signature                  @14 :GrantSignature       $Traits.doc("Issuer signature binding every other field of this grant; absent only when the caller is the issuer. See wire/run-grant.md.") $Traits.optional;
}

struct ExecutionError $Traits.doc("Stable transport-independent execution error.") {
  code      @0 :ErrorCode $Traits.doc("Machine-readable classification.");
  message   @1 :Text      $Traits.doc("Safe operator-facing message with no secrets.");
  retryable @2 :Bool      $Traits.doc("Whether retry under the same authority may succeed.");
  detailRef @3 :DigestRef $Traits.doc("Optional content-addressed diagnostic detail; empty digest means absent.") $Traits.optional;
}

struct RunEvent $Traits.doc("One append-only lifecycle event.") {
  sequence      @0 :UInt64    $Traits.doc("Monotonic per-run event sequence.");
  runId         @1 :Text      $Traits.doc("Run identifier.");
  state         @2 :RunState  $Traits.doc("Lifecycle state after this event.");
  timestampMs   @3 :UInt64    $Traits.doc("Substrate-observed Unix timestamp in milliseconds.");
  detailDigest  @4 :DigestRef $Traits.doc("Optional event detail digest; empty digest means absent.") $Traits.optional;
}

struct ArtifactResult $Traits.doc("Collected named output.") {
  name     @0 :Text        $Traits.doc("Output declaration name.");
  artifact @1 :ArtifactRef $Traits.doc("Immutable collected artifact.");
}

struct ResourceUsage $Traits.doc("Terminal resource accounting.") {
  wallTimeMs  @0 :UInt64 $Traits.doc("Observed wall-clock runtime.");
  cpuMillis   @1 :UInt64 $Traits.doc("Observed cumulative CPU time.");
  peakMemory  @2 :UInt64 $Traits.doc("Observed peak memory bytes.");
  outputBytes @3 :UInt64 $Traits.doc("Collected output bytes.");
}

struct BackendEvidence $Traits.doc("Implementation and enforcement evidence for the selected backend class.") {
  backendClass @0 :BackendClass $Traits.doc("Isolation class actually used.");
  backendId    @1 :Text         $Traits.doc("Versioned implementation identifier.");
  evidence     @2 :EvidenceRef  $Traits.doc("Content-addressed backend enforcement evidence.");
}

struct RunReceipt $Traits.doc("Terminal substrate evidence; may be embedded by a separate APAS attester.") {
  schemaVersion              @0  :Text                 $Traits.doc("Receipt contract identifier.");
  runId                      @1  :Text                 $Traits.doc("Run identifier.");
  terminalState              @2  :RunState             $Traits.doc("Terminal lifecycle state.");
  eventLogRoot               @3  :DigestRef            $Traits.doc("Root of the ordered event log.");
  runSpecDigest              @4  :DigestRef            $Traits.doc("Executed intent digest.");
  runGrantDigest             @5  :DigestRef            $Traits.doc("Verified authority digest.");
  confinementDigest         @6  :DigestRef            $Traits.doc("Policy digest actually enforced.");
  workloadIdentityEvidence  @7  :EvidenceRef           $Traits.doc("Workload identity evidence used for authorization.");
  actorProvenanceEvidence   @8  :EvidenceRef           $Traits.doc("Delegated actor/provenance evidence used for authorization.");
  backend                    @9  :BackendEvidence       $Traits.doc("Backend identity and enforcement evidence.");
  inputRoots                 @10 :List(DigestRef)       $Traits.doc("Content roots materialized for the run.");
  outputs                    @11 :List(ArtifactResult)  $Traits.doc("Declared collected outputs only.");
  usage                      @12 :ResourceUsage         $Traits.doc("Terminal resource accounting.");
  startedAtUnixMs           @13 :UInt64                $Traits.doc("Substrate-observed start time.");
  completedAtUnixMs         @14 :UInt64                $Traits.doc("Substrate-observed terminal time.");
}

struct CapabilityDescriptor $Traits.doc("One supported execution interface or backend class.") {
  name    @0 :Text $Traits.doc("Stable interface or backend identifier.");
  version @1 :Text $Traits.doc("Implementation or contract version.");
}

struct CapabilitiesInput
  $Traits.doc("Discover execution interfaces and backend classes without mutating host state.")
  $Traits.op((input = "CapabilitiesInput", output = "CapabilitiesOutput", errors = ["ExecutionError"], name = "llo_execution_capabilities")) {
}

struct CapabilitiesOutput $Traits.doc("Read-only execution capability discovery result.") {
  capabilities @0 :List(CapabilityDescriptor) $Traits.doc("Supported interfaces and backend classes.");
}

struct StatusInput
  $Traits.doc("Read substrate readiness without provisioning or creating storage.")
  $Traits.op((input = "StatusInput", output = "StatusOutput", errors = ["ExecutionError"], name = "llo_execution_status")) {
  runId @0 :Text $Traits.doc("Optional run to inspect; empty means substrate status.") $Traits.optional;
}

struct StatusOutput $Traits.doc("Read-only substrate or run status.") {
  provisioned @0 :Bool     $Traits.doc("Whether the selected backend is explicitly provisioned.");
  backend     @1 :Text     $Traits.doc("Selected backend implementation identifier; empty when unavailable.");
  runId       @2 :Text     $Traits.doc("Run identifier; empty for substrate status.");
  state       @3 :RunState $Traits.doc("Current state; accepted is the neutral value for substrate-only status.");
  lastEvent   @4 :UInt64   $Traits.doc("Latest observed event sequence; zero when no run was selected.");
}

struct ProvisionInput
  $Traits.doc("Explicitly and idempotently provision execution storage or backend resources.")
  $Traits.op((input = "ProvisionInput", output = "ProvisionOutput", errors = ["ExecutionError"], name = "llo_execution_provision")) {
  backendClass   @0 :BackendClass $Traits.doc("Isolation class to provision.");
  idempotencyKey @1 :Text         $Traits.doc("Caller-stable retry key.");
}

struct ProvisionOutput $Traits.doc("Provisioning result.") {
  provisioned @0 :Bool $Traits.doc("True when the backend is ready.");
  backendId   @1 :Text $Traits.doc("Selected backend implementation identifier.");
}

struct StartInput
  $Traits.doc("Verify a resolved grant, materialize only its capabilities, and start one run.")
  $Traits.op((input = "StartInput", output = "StartOutput", errors = ["ExecutionError"], name = "llo_execution_start")) {
  spec  @0 :RunSpec  $Traits.doc("Untrusted execution intent.");
  grant @1 :RunGrant $Traits.doc("Authenticated resolved authority bound to spec.");
}

struct StartOutput $Traits.doc("Accepted run identity and initial state.") {
  runId @0 :Text     $Traits.doc("Stable run identifier.");
  state @1 :RunState $Traits.doc("Initial state after acceptance.");
}

struct InspectInput
  $Traits.doc("Read one run and its append-only events.")
  $Traits.op((input = "InspectInput", output = "InspectOutput", errors = ["ExecutionError"], name = "llo_execution_inspect")) {
  runId        @0 :Text   $Traits.doc("Run identifier.");
  afterSequence @1 :UInt64 $Traits.doc("Return events strictly after this sequence.") $Traits.optional;
}

struct InspectOutput $Traits.doc("Current run state and ordered new events.") {
  runId @0 :Text           $Traits.doc("Run identifier.");
  state @1 :RunState       $Traits.doc("Current lifecycle state.");
  events @2 :List(RunEvent) $Traits.doc("Events after the requested sequence.");
}

struct CancelInput
  $Traits.doc("Request cancellation using the run capability.")
  $Traits.op((input = "CancelInput", output = "CancelOutput", errors = ["ExecutionError"], name = "llo_execution_cancel")) {
  runId          @0 :Text $Traits.doc("Run identifier.");
  idempotencyKey @1 :Text $Traits.doc("Caller-stable retry key.");
}

struct CancelOutput $Traits.doc("Cancellation request result.") {
  runId @0 :Text     $Traits.doc("Run identifier.");
  state @1 :RunState $Traits.doc("Observed state after the request.");
}

struct CollectInput
  $Traits.doc("Collect declared outputs and a terminal receipt.")
  $Traits.op((input = "CollectInput", output = "CollectOutput", errors = ["ExecutionError"], name = "llo_execution_collect")) {
  runId @0 :Text $Traits.doc("Terminal run identifier.");
}

struct CollectOutput $Traits.doc("Declared outputs and terminal receipt.") {
  outputs @0 :List(ArtifactResult) $Traits.doc("Declared collected outputs.");
  receipt @1 :RunReceipt           $Traits.doc("Terminal substrate receipt.");
}

struct CleanupInput
  $Traits.doc("Idempotently release one run's ephemeral resources.")
  $Traits.op((input = "CleanupInput", output = "CleanupOutput", errors = ["ExecutionError"], name = "llo_execution_cleanup")) {
  runId          @0 :Text $Traits.doc("Run identifier.");
  idempotencyKey @1 :Text $Traits.doc("Caller-stable retry key.");
}

struct CleanupOutput $Traits.doc("Cleanup result.") {
  runId @0 :Text     $Traits.doc("Run identifier.");
  state @1 :RunState $Traits.doc("cleaned when all ephemeral resources are released.");
}
