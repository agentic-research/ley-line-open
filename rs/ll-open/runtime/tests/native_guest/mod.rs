//! What a test needs to run a copy of its own binary as a guest of the
//! first-party native nono worker. Shared by `native_confinement.rs` and the
//! native column of the conformance matrix (`conformance_matrix.rs`).

#![allow(dead_code)] // each test binary uses a different subset

use std::path::PathBuf;

use leyline_runtime::ExecutionError;

/// The host files the guest needs to execute at all, granted read-only as the
/// worker's `--runtime-file`s.
///
/// macOS: none. dyld and the system libraries come from the shared cache,
/// which Seatbelt does not mediate as file reads, so the guest runs with the
/// rootfs as its only grant.
///
/// Linux: the guest is a copy of this dynamically linked test binary, and
/// Landlock denies the loader and libc like any other path, so the guest
/// could not start without them. The shared objects mapped into THIS process
/// are exactly what the identical guest binary loads; they are granted
/// file-by-file, which leaves every decoy (none is a mapped library) denied.
pub fn guest_runtime_files() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        let exe = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .expect("test executable");
        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        let mut files = std::collections::BTreeSet::new();
        for line in maps.lines() {
            let Some(path) = line.split_whitespace().nth(5) else {
                continue;
            };
            let path = Path::new(path);
            if path.is_absolute()
                && path.is_file()
                && let Ok(canonical) = path.canonicalize()
                && canonical != exe
            {
                files.insert(canonical);
            }
        }
        // The loader consults its cache first; without it, it still finds the
        // libraries on its default path, but the denied open is noise.
        if Path::new("/etc/ld.so.cache").is_file() {
            files.insert(PathBuf::from("/etc/ld.so.cache"));
        }
        files.into_iter().collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// `~/.ssh`, when this host has one: the AC's named example of undeclared
/// host access.
pub fn home_ssh() -> Option<PathBuf> {
    let ssh = PathBuf::from(std::env::var_os("HOME")?).join(".ssh");
    ssh.is_dir().then_some(ssh)
}

/// Whether a worker error means the host could not apply the sandbox at all
/// (an outer sandbox, a kernel without Landlock) rather than a failure.
pub fn is_apply_failure(error: &ExecutionError) -> bool {
    error.detail.contains("apply native nono confinement")
}
