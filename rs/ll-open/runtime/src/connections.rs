//! A run's connections as values (ADR-0046 Slice 1, S3, S6, S8; bead
//! `ley-line-open-5308b7`).
//!
//! - [`VerifiedConnections`]: one canonical `cloister/connections/v1`
//!   document and its digest. It is built only from a parsed document, never
//!   from raw fields, so the digest always names the document beside it.
//! - [`RunConnections`]: what [`crate::Backend::start`] is handed. An
//!   embedder states [`RunConnections::none`] or
//!   [`RunConnections::verified`]; [`crate::Brokered`] turns a verified value
//!   into a served one before the backend it wraps is called.
//! - [`ServedConnections`]: the run's broker is bound, confined and ready. A
//!   backend's only duty is to name its worker to it, once.
//! - [`ConnectionsOutcome`]: what the run's receipt records.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use leyline_schema_spec::connections::ConnectionsDocument;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::ExecutionError;
use crate::broker::control::{BrokerProcess, ConnectionSummary};

/// One authorized `cloister/connections/v1` document and the
/// `connectionsDigest` of its canonical bytes.
///
/// The fields are private, and the only ways in are a parsed
/// [`ConnectionsDocument`] and canonical bytes:
///
/// ```compile_fail,E0451
/// let forged = leyline_runtime::VerifiedConnections {
///     document: unimplemented!(),
///     digest: String::new(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedConnections {
    document: ConnectionsDocument,
    digest: String,
}

impl VerifiedConnections {
    /// From a document the schema crate built (`parse` or
    /// `ConnectionsDocument::new`); the digest is taken here, over its
    /// canonical bytes.
    pub fn new(document: ConnectionsDocument) -> Self {
        let digest = format!(
            "{}:{}",
            leyline_schema_spec::connections::DIGEST_ALGORITHM,
            document.digest_hex()
        );
        Self { document, digest }
    }

    /// From the canonical bytes of a document. Bytes that parse but are not
    /// their own canonical form are refused, never normalized, the rule
    /// `RunGrant.connectionsDocument` is held to.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, ExecutionError> {
        let document = leyline_schema_spec::connections::parse(bytes).map_err(|error| {
            ExecutionError::invalid_grant(format!(
                "not a valid cloister/connections/v1 document: {error}"
            ))
        })?;
        if document.canonical_bytes() != bytes {
            return Err(ExecutionError::invalid_grant(
                "the connections document is not in canonical form; it is refused rather than normalized",
            ));
        }
        Ok(Self::new(document))
    }

    pub fn document(&self) -> &ConnectionsDocument {
        &self.document
    }

    /// `blake3-256:<hex>` over the document's canonical bytes.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl From<crate::authorization::AuthorizedConnections> for VerifiedConnections {
    /// The digest is recomputed from the document, so a value built from an
    /// authorization result cannot carry a digest of other bytes.
    fn from(authorized: crate::authorization::AuthorizedConnections) -> Self {
        Self::new(authorized.document)
    }
}

/// The connections [`crate::Backend::start`] is handed for one run.
pub struct RunConnections {
    state: State,
}

pub(crate) enum State {
    None,
    Verified(VerifiedConnections),
    Served(ServedConnections),
}

impl RunConnections {
    /// A run that declares no connection.
    pub fn none() -> Self {
        Self { state: State::None }
    }

    /// A run that declares `connections`. Only [`crate::Brokered`] (which
    /// [`crate::ExecutionService`] is built on) serves them; a backend handed
    /// this value directly refuses the run by name.
    pub fn verified(connections: VerifiedConnections) -> Self {
        Self {
            state: State::Verified(connections),
        }
    }

    pub(crate) fn served(served: ServedConnections) -> Self {
        Self {
            state: State::Served(served),
        }
    }

    pub(crate) fn into_state(self) -> State {
        self.state
    }

    /// What a backend takes before it spawns anything: the served
    /// connections of `request`'s run, or `None` for a run without any.
    ///
    /// Refused by name when the connections are declared and nothing serves
    /// them (the backend was called without [`crate::Brokered`]), and when
    /// they were served for another run. A backend that ignored the value
    /// instead would start a run whose declared connections do not exist.
    pub fn served_for(
        self,
        request: &crate::ExecutionRequest,
        backend_id: &str,
    ) -> Result<Option<ServedConnections>, ExecutionError> {
        match self.state {
            State::None => Ok(None),
            State::Verified(connections) => Err(ExecutionError::unsupported(format!(
                "connection `{}`: backend {backend_id} was handed the run's connections \
                 with no broker serving them; start the run through ExecutionService or \
                 leyline_runtime::Brokered",
                first_name(connections.document())
            ))),
            State::Served(served) if served.run_id == request.run_id => Ok(Some(served)),
            State::Served(served) => Err(ExecutionError::identity_mismatch(format!(
                "backend {backend_id} was handed connections served for run {:?}, and the \
                 request is run {:?}",
                served.run_id, request.run_id
            ))),
        }
    }
}

/// The name of a document's first connection, for a refusal that concerns
/// the whole document. A document has at least one.
pub(crate) fn first_name(document: &ConnectionsDocument) -> &str {
    document
        .connections()
        .first()
        .map_or("", |connection| connection.name.as_str())
}

/// One run's connections, served: its broker has bound every endpoint,
/// confined itself and attested the document's digest (ADR-0046 S7 step 4).
///
/// On the microVM tier the broker accepts no peer until the backend names
/// its worker, the process that hosts the VMM ([`Self::name_worker`]).
pub struct ServedConnections {
    pub(crate) run_id: String,
    pub(crate) digest: String,
    pub(crate) broker: Arc<Mutex<BrokerProcess>>,
    /// Whether the broker was started awaiting its one peer.
    pub(crate) awaits_worker: bool,
    pub(crate) named: Arc<AtomicBool>,
}

impl ServedConnections {
    /// The `connectionsDigest` the broker attested.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Name the worker process, the only peer the broker then accepts.
    /// Called once, as soon as the worker is spawned and before its request
    /// is sent, so no guest connection can arrive first. A second call is
    /// refused: the peer is never changed.
    pub fn name_worker(&self, pid: u32) -> Result<(), ExecutionError> {
        if self.named.swap(true, Ordering::SeqCst) {
            return Err(ExecutionError::backend(
                "the run's worker was already named to its connection broker",
            ));
        }
        if !self.awaits_worker {
            return Ok(());
        }
        let pid = i32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| ExecutionError::backend(format!("{pid} is not a worker process id")))?;
        self.broker.lock().set_peer(pid)
    }
}

/// What a run's connections carried, as the supervisor assembled it from the
/// broker's records (RunReceipt `connectionsDigest @15`, `connections @16`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsOutcome {
    /// The digest the broker attested in its ready report, `blake3-256:<hex>`.
    pub digest: String,
    /// One summary per declared connection, in document order.
    pub connections: Vec<ConnectionSummary>,
}

/// That a backend serves connections, and at which revision of the
/// extension (ADR-0046 S3, "Capability discovery"). Absent means refuse: a
/// caller never infers it from a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsCapability {
    pub version: String,
}

impl ConnectionsCapability {
    /// The capability's name in `llo_execution_capabilities`.
    pub const NAME: &'static str = "cloister/execution/v1/connections";
    /// The revision of the extension's layout this runtime implements.
    pub const REVISION: &'static str = "1";

    pub fn current() -> Self {
        Self {
            version: Self::REVISION.into(),
        }
    }
}
