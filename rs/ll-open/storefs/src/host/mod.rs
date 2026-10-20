//! The host mount (bead `ley-line-open-3e73dc`, ADR-0045 T8): [`StoreFs`]
//! behind the host kernel's FUSE or NFSv3 client, so a host process reads
//! and writes the store as a guest does through its virtio-fs share. One
//! filesystem, one set of rules: every file, mode, symlink and directory
//! `_tree` holds; each save a proposal on the daemon's one writer; a moved
//! base merged three ways; an overlap `ESTALE`.
//!
//! macOS mounts NFSv3 from a loopback server (hf-mount's arrangement, on
//! xetdata's `nfsserve`), since FUSE there needs macFUSE. Linux mounts FUSE.

pub mod fuse;
pub mod nfs;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use leyline_cli_lib::cmd_daemon::{HostMount, MountGuard};
use leyline_cli_lib::daemon::DaemonContext;
use leyline_cli_lib::source_write::Grant;
use nfsserve::tcp::{NFSTcp, NFSTcpListener};

use crate::{Checkout, Freshness, Options, Scope, Store, StoreFs};

/// How often the NFS server looks for a truncate that has stood for
/// [`nfs::COMMIT_IDLE`].
const COMMIT_TICK: Duration = Duration::from_millis(50);

/// The NFS client's attribute cache, in seconds (`actimeo`). NFS names no
/// per-entry timeout, so the whole mount is held to [`Freshness`]'s timeout
/// for a recently changed path; every `open` also revalidates the file
/// (NFS close-to-open consistency), which is ADR-0044 V3's fresh-at-open.
const NFS_ATTR_CACHE_SECS: u64 = 1;

/// Where the NFS export listens: loopback, on `port` (0 picks one). The
/// export is unauthenticated and writable, and [`mount_nfs`] reaches it at
/// 127.0.0.1, so nothing off this host has a reason to reach it.
pub fn nfs_listen_addr(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// A running NFS server. Dropping it stops the server and lands everything
/// staged before the drop returns, so whoever drops it (the mount guard,
/// after unmounting) can publish knowing the last write is in.
pub struct NfsServer<S: Store + 'static> {
    port: u16,
    fs: nfs::HostNfs<S>,
    task: tokio::task::JoinHandle<()>,
}

impl<S: Store + 'static> NfsServer<S> {
    /// The port the server bound (the one asked for, or the one picked for 0).
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl<S: Store + 'static> Drop for NfsServer<S> {
    fn drop(&mut self) {
        self.task.abort();
        self.fs.commit_all();
    }
}

/// Serve `fs` over NFSv3 at `listen_addr` until the returned server is
/// dropped.
pub async fn serve_nfs<S: Store + 'static>(
    fs: Arc<StoreFs<S>>,
    listen_addr: &str,
) -> Result<NfsServer<S>> {
    let host = nfs::HostNfs::new(fs);
    let listener = NFSTcpListener::bind(listen_addr, host.clone()).await?;
    let port = listener.get_listen_port();
    let idle = host.clone();
    let task = tokio::spawn(async move {
        // The idle committer runs beside the server and ends with it.
        let committer = async {
            let mut tick = tokio::time::interval(COMMIT_TICK);
            loop {
                tick.tick().await;
                idle.commit_if_idle(Instant::now(), nfs::COMMIT_IDLE);
            }
        };
        tokio::select! {
            served = listener.handle_forever() => {
                if let Err(e) = served {
                    log::error!("NFS server error: {e}");
                }
            }
            () = committer => {}
        }
    });
    Ok(NfsServer {
        port,
        fs: host,
        task,
    })
}

/// Whether a macOS release's NFS client is safe to mount a loopback server
/// with, from its product version (`26.7`, `15.8.1`, `27.0.1`). Before Tahoe
/// 26.7 and Sequoia 15.8 the client corrupts kernel memory on server replies
/// it mishandles (CVE-2026-43686, a use-after-free; CVE-2026-84549, an
/// out-of-bounds read); a loopback NFS workload panicked a development Mac
/// in `com.apple.filesystems.nfs` (bead `ley-line-open-2d49ce`). The answer
/// is the refusal to give, or `None` for a release with both fixes.
pub fn unpatched_nfs_client(product_version: &str) -> Option<String> {
    let mut parts = product_version.trim().split('.');
    let mut number = || parts.next().and_then(|p| p.parse::<u32>().ok());
    let (major, minor) = (number(), number().unwrap_or(0));
    let patched = match major {
        Some(15) => minor >= 8,
        Some(26) => minor >= 7,
        Some(major) => major >= 27,
        None => false,
    };
    (!patched).then(|| {
        format!(
            "refusing to mount NFS on macOS {}: its NFS client corrupts kernel memory on \
             server replies (CVE-2026-43686, CVE-2026-84549), fixed in macOS Tahoe 26.7 and \
             Sequoia 15.8. Update macOS, or mount with --backend fuse (macFUSE or fuse-t).",
            product_version.trim()
        )
    })
}

/// This Mac's product version (`kern.osproductversion`).
#[cfg(target_os = "macos")]
fn macos_product_version() -> Result<String> {
    let name = c"kern.osproductversion";
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `buf` is valid for `len` bytes and `sysctlbyname` writes at
    // most `len` bytes into it, updating `len` to the count written; the
    // name is NUL-terminated and no new value is set.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("read kern.osproductversion");
    }
    let bytes = &buf[..len];
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// An NFS mount this process made and the server behind it. Dropping it
/// unmounts (the kernel flushes the client's last writes to the server) and
/// then drops the server, which lands them before the drop returns.
pub struct NfsMount<S: Store + 'static> {
    path: PathBuf,
    /// [`unmount_nfs`] in every real mount; a test substitutes a recorder.
    unmount: fn(&Path) -> Result<()>,
    /// Dropped after `Drop::drop` has unmounted (fields drop after it).
    _server: NfsServer<S>,
}

impl<S: Store + 'static> Drop for NfsMount<S> {
    fn drop(&mut self) {
        if let Err(e) = (self.unmount)(&self.path) {
            eprintln!("warn: {e:#}");
        }
    }
}

/// Unmount an NFS mount: `umount`, then `umount -f` if the first refuses.
pub fn unmount_nfs(path: &Path) -> Result<()> {
    for args in [&[][..], &["-f"][..]] {
        let status = std::process::Command::new("umount")
            .args(args)
            .arg(path)
            .status()
            .context("spawn umount")?;
        if status.success() {
            return Ok(());
        }
    }
    anyhow::bail!("could not unmount {}", path.display())
}

/// The kernel's NFS client mounts `server`'s export at `mountpoint`
/// (`mount_nfs` on macOS, `mount.nfs` elsewhere). On macOS a release whose
/// NFS client is unpatched is refused ([`unpatched_nfs_client`]).
pub fn mount_nfs<S: Store + 'static>(
    server: NfsServer<S>,
    mountpoint: &Path,
) -> Result<NfsMount<S>> {
    #[cfg(target_os = "macos")]
    if let Some(refusal) = unpatched_nfs_client(&macos_product_version()?) {
        anyhow::bail!(refusal);
    }
    let port = server.port();
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("create mountpoint {}", mountpoint.display()))?;
    let mount_path = mountpoint
        .canonicalize()
        .unwrap_or_else(|_| mountpoint.to_path_buf());
    let nfs_opts = format!(
        "nolocks,vers=3,tcp,rsize=131072,wsize=131072,actimeo={NFS_ATTR_CACHE_SECS},\
         port={port},mountport={port}"
    );
    let mount_cmd = if cfg!(target_os = "macos") {
        "mount_nfs"
    } else {
        "mount.nfs"
    };
    let status = std::process::Command::new(mount_cmd)
        .args(["-o", &nfs_opts, "127.0.0.1:/"])
        .arg(&mount_path)
        .status()
        .with_context(|| format!("spawn {mount_cmd}"))?;
    if !status.success() {
        anyhow::bail!("NFS mount failed with exit code {:?}", status.code());
    }
    Ok(NfsMount {
        path: mount_path,
        unmount: unmount_nfs,
        _server: server,
    })
}

/// A FUSE session; dropping it unmounts.
pub type FuseSession = ::fuser::BackgroundSession;

/// Mount `fs` over FUSE at `mountpoint`. No `auto_unmount`: on libfuse 2
/// that option makes `fusermount` add `allow_other`, which a stock Linux
/// refuses without `user_allow_other` in `/etc/fuse.conf`. The session's
/// `Drop` is the unmount.
pub fn mount_fuse<S: Store + 'static>(
    fs: Arc<StoreFs<S>>,
    mountpoint: &Path,
) -> Result<FuseSession> {
    let options = vec![::fuser::MountOption::FSName("leyline".into())];
    Ok(::fuser::spawn_mount2(
        fuse::HostFuse::new(fs),
        mountpoint,
        &options,
    )?)
}

/// A mount this process holds while it serves; dropping it unmounts.
pub enum Mounted<S: Store + 'static> {
    Nfs(NfsMount<S>),
    Fuse(FuseSession),
}

/// Mount `fs` at `mount` on `backend` (`nfs` or `fuse`).
pub async fn mount<S: Store + 'static>(
    fs: Arc<StoreFs<S>>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
) -> Result<Mounted<S>> {
    std::fs::create_dir_all(mount)
        .with_context(|| format!("create mountpoint {}", mount.display()))?;
    match backend {
        "nfs" => {
            let server = serve_nfs(fs, &nfs_listen_addr(nfs_port)).await?;
            log::info!("NFS server listening on port {}", server.port());
            Ok(Mounted::Nfs(mount_nfs(server, mount)?))
        }
        "fuse" => Ok(Mounted::Fuse(mount_fuse(fs, mount)?)),
        other => anyhow::bail!("unknown backend: {other} (expected 'nfs' or 'fuse')"),
    }
}

/// The daemon's host mount: the store behind the daemon's own reader pool
/// and one writer, the whole tree seen and proposable, each landed proposal
/// written into the daemon's checkout and published to its arena.
pub struct DaemonHostMount {
    freshness: Freshness,
}

impl DaemonHostMount {
    pub fn new(freshness: Freshness) -> Self {
        Self { freshness }
    }
}

impl HostMount for DaemonHostMount {
    fn mount<'a>(
        &'a self,
        ctx: Arc<DaemonContext>,
        at: &'a Path,
        backend: &'a str,
        nfs_port: u16,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<MountGuard>> + Send + 'a>> {
        Box::pin(async move {
            let options = Options {
                freshness: self.freshness,
                checkout: ctx.source_dir.clone().map(|dir| Checkout {
                    dir,
                    lang_filter: ctx.lang_filter.clone(),
                }),
            };
            let scope = Scope {
                see: Grant::Whole,
                propose: Grant::Whole,
            };
            let fs = Arc::new(StoreFs::new(ctx, scope, options));
            let mounted = mount(fs, at, backend, nfs_port).await?;
            Ok(Box::new(mounted) as MountGuard)
        })
    }
}

#[cfg(test)]
mod tests;
