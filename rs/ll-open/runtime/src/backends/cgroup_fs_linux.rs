//! `CgroupRoot::open`'s Linux step: the filesystem magic of the root, from
//! `statfs(2)`. Excluded from the macOS mutation slice, which cannot compile
//! it; mutated on Linux, where `cgroup.rs`'s tests open `/sys/fs/cgroup`.

use std::path::Path;

use crate::ExecutionError;

pub(super) fn filesystem_magic(path: &Path) -> Result<u64, ExecutionError> {
    let stat = rustix::fs::statfs(path).map_err(|error| {
        ExecutionError::backend(format!("cgroup root {}: statfs: {error}", path.display()))
    })?;
    // `f_type`'s integer type differs by architecture; every magic fits u64.
    #[allow(clippy::unnecessary_cast)]
    let magic = stat.f_type as u64;
    Ok(magic)
}
