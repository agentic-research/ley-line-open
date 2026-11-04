//! # leyline-cache
//!
//! Restriction-addressed derived-view caching (ADR-0031). A content address
//! says whether two stored objects are identical; it cannot say whether an
//! expensive derived view over several objects (a resolved call graph, a
//! public-API surface, a review verdict) is still valid without recomputing
//! it. A restriction key can: it is an address over exactly the substrate rows
//! the view observes, so it is unchanged under edits outside that closure and
//! changes under any edit inside it.
//!
//! Moved here from `leyline-sheaf::restriction_cache` (bead
//! `ley-line-open-754aa1`). It shares no code with the sheaf engine and needs
//! none of its dependencies.

pub mod restriction;
pub mod strategy;
