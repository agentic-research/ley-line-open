//! `CgroupRoot::open`'s step on every platform but Linux: there is no cgroup
//! v2, so no root. Excluded from the Linux mutation slice, which cannot
//! compile it; mutated elsewhere, where `cgroup.rs`'s tests assert the
//! refusal.

use std::path::Path;

use crate::ExecutionError;

pub(super) fn filesystem_magic(path: &Path) -> Result<u64, ExecutionError> {
    Err(ExecutionError::backend(format!(
        "cgroup root {}: cgroup v2 exists only on Linux",
        path.display()
    )))
}
