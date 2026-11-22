//! gix-merge 0.17.0's builtin text merge driver, vendored.
//!
//! `src/blob/builtin_driver/text/` is upstream's `src/blob/builtin_driver/text/`
//! byte for byte (gix-merge 0.17.0 by the gitoxide authors, MIT OR
//! Apache-2.0: `LICENSE-MIT`, `LICENSE-APACHE`). This file declares the module path those files name and upstream's
//! `blob::Resolution`, copied from upstream's `src/blob/mod.rs`. Nothing else
//! of gix-merge is here.
#![allow(missing_docs)]

/// Module path mirroring upstream's `gix_merge::blob`.
pub mod blob {
    /// Define if a merge is conflicted or not.
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub enum Resolution {
        /// Everything could be resolved during the merge, and there was no conflict.
        Complete,
        /// Conflicts were resolved automatically, even thought the result is complete
        /// and free of conflict markers.
        /// This can only be the case for text-file content merges.
        CompleteWithAutoResolvedConflict,
        /// A conflict is still present in the form of conflict markers.
        Conflict,
    }

    /// Module path mirroring upstream's `gix_merge::blob::builtin_driver`.
    pub mod builtin_driver {
        pub mod text;
        pub use text::function::merge as text;
    }
}
