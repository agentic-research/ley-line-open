//! **F1 — Crash mid-write leaves reader observing only the COMPLETE
//! prior root.**
//!
//! Falsifies substrate axiom (IM) and the R4 crash-consistency
//! requirement (decade `docs/decades/2026-merkle-cas-substrate.md` §4).
//!
//! ## Claim (from the decade)
//!
//! > Crash mid-snapshot leaves the reader observing only the COMPLETE
//! > prior root; no torn state visible.
//!
//! ## Test shape
//!
//! `snapshot_to_arena` (`cli-lib/src/cmd_daemon.rs`) has a load-bearing
//! ordering contract: file is grown, size is re-advertised via
//! `Controller::set_arena` (preserves `current_root`), new bytes are
//! written into the inactive buffer, and only then the root is
//! advanced via `Controller::set_arena_with_root`. The window between
//! set_arena (early publish) and set_arena_with_root (final publish)
//! is the crash-vulnerable region: if a crash there let the reader
//! see a NEW root pointing at a HALF-written buffer, (IM) would be
//! broken.
//!
//! Two crash-window flavors, one per `#[test]`:
//!
//! - `crash_after_reAdvertise_same_size_preserves_root_and_bytes`:
//!   The child re-advertises the arena with SAME size (models the
//!   snapshot_to_arena skip-early-set_arena path when `new_size ==
//!   arena_size`). The file geometry is stable so a fresh reader
//!   can hash-verify the addressed bytes against `R0` — the strongest
//!   "no torn data" assertion.
//! - `crash_after_grow_and_reAdvertise_larger_size_preserves_root`:
//!   The child GROWS the file and re-advertises the larger size —
//!   the full mid-snapshot state. `current_root` is preserved and
//!   `file_size >= arena_size` (grow-before-advertise ordering). A
//!   fresh reader mid-grow can't hash-verify bytes at the new
//!   geometry (buffer_size derived from post-grow file_size gives an
//!   offset that OLD bytes never populated), but such a reader is
//!   protected by the substrate's verify-on-read pattern which
//!   rejects any σ-mismatch. The load-bearing property is: no
//!   premature root advance — which this test asserts directly.
//!
//! ## Pass criteria (per flavor)
//!
//! Same-size flavor:
//! - Reader sees `current_root == R0` (root unchanged).
//! - Bytes at active buffer hash to `R0` (verify-on-read succeeds).
//! - File size == advertised size (unchanged).
//!
//! Grow-and-advertise flavor:
//! - Reader sees `current_root == R0` (root unchanged).
//! - File size ≥ advertised size (grow-before-advertise ordering).
//! - Advertised size == the newly-grown size (writer's set_arena
//!   completed).
//! - Bytes at the ORIGINAL buffer offset (pre-grow geometry) still
//!   equal `V0` (grow didn't overwrite pre-existing data).
//!
//! ## Unix-only
//!
//! Uses `libc::fork` + `libc::kill(SIGKILL)` + `libc::waitpid`. Windows
//! and other non-unix hosts skip the test. `libc` is a direct dep on
//! unix per bead `ley-line-open-0cba88`'s admission-control work.

#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use leyline_core::Controller;
use leyline_core::layout::{ArenaHeader, create_arena, write_to_arena};
use leyline_core::substrate::{ContentAddressed, Hash};
use tempfile::TempDir;

/// Size of the initial arena (3 pages = header + 2×4KB buffers).
const INIT_ARENA_SIZE: u64 = 4096 + 4096 * 2;

/// Size of the grown arena the child re-advertises in the grow flavor.
const GROWN_ARENA_SIZE: u64 = 4096 + 4096 * 30;

/// Deadline for the child to signal ready. Long enough that CI noise
/// never spuriously flakes; short enough that a genuine wedge fails
/// fast.
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Sentinel exit code the child returns if pause() somehow returns
/// without SIGKILL. If waitpid observes THIS code, the child completed
/// too much work and the test is no longer opening the crash window.
const CHILD_UNEXPECTED_EXIT_CODE: i32 = 99;

/// Which side of the mid-snapshot window the child stops at.
#[derive(Copy, Clone)]
enum CrashFlavor {
    /// Re-advertise the arena at the same size. File geometry stable —
    /// reader can hash-verify addressed bytes.
    SameSizeReAdvertise,
    /// Grow the file first, then re-advertise at the larger size. File
    /// geometry changed — reader verifies root + grow-before-advertise
    /// ordering, but does NOT hash-verify bytes (fresh reader mid-grow
    /// can't; polling reader would use its cached OLD mmap).
    GrowAndReAdvertise,
}

#[test]
fn crash_after_re_advertise_same_size_preserves_root_and_bytes() {
    run_flavor(CrashFlavor::SameSizeReAdvertise);
}

#[test]
fn crash_after_grow_and_re_advertise_larger_size_preserves_root() {
    run_flavor(CrashFlavor::GrowAndReAdvertise);
}

fn run_flavor(flavor: CrashFlavor) {
    let td = TempDir::new().expect("tempdir");
    let arena_path = td.path().join("f1.arena");
    let ctrl_path = td.path().join("f1.ctrl");
    let ready_path = td.path().join("child.ready");

    // ── Parent: seed a committed root R0 ───────────────────────────────
    //
    // Establish the "prior generation" state: initial arena buffer
    // holds bytes V0, the controller publishes σ(V0) = R0. This is
    // the root the reader should observe after the child crashes.
    let v0: Vec<u8> = b"F1 initial content pre-crash - version 0 payload".to_vec();
    let r0: Hash = v0.as_slice().hash();

    {
        let mut mmap = create_arena(&arena_path, INIT_ARENA_SIZE).expect("create initial arena");
        write_to_arena(&mut mmap, &v0).expect("write V0 to inactive buffer + flip active");
        drop(mmap);

        let mut ctrl = Controller::open_or_create(&ctrl_path).expect("open controller");
        ctrl.set_arena_with_root(
            &arena_path.to_string_lossy(),
            INIT_ARENA_SIZE,
            *r0.as_bytes(),
        )
        .expect("publish R0");
    }

    // Sanity: reader opened fresh sees R0 and V0 verify-on-reads at
    // the initial geometry.
    assert_reader_sees_root_at_init_geometry(&arena_path, &ctrl_path, &r0, &v0);

    // ── Fork ───────────────────────────────────────────────────────────
    //
    // The child does exactly the pre-crash portion of snapshot_to_arena's
    // ordering, then signals ready and blocks. The parent kills it
    // before it can reach set_arena_with_root.
    //
    // SAFETY: fork() in a multithreaded process is dangerous because
    // only the calling thread survives in the child. This test binary
    // is single-threaded at the fork point — no tokio runtime, no
    // rayon pool, no thread::spawn — so the fork is safe.
    let pid = unsafe { libc::fork() };
    assert!(
        pid >= 0,
        "fork() failed: errno {}",
        std::io::Error::last_os_error()
    );

    if pid == 0 {
        // ── Child ────────────────────────────────────────────────────
        //
        // Any failure inside the child MUST NOT panic-unwind into the
        // parent's harness. On success we sleep and get SIGKILLed. On
        // failure we `_exit` with a distinct code so the parent's
        // waitpid observes it and fails the test with a diagnostic.
        //
        // `_exit` (not `exit`) skips Rust drops — mmap flush /
        // TempDir cleanup would collide with the parent's state.
        let rc = child_perform_pre_crash_setup(&arena_path, &ctrl_path, &ready_path, flavor);
        unsafe { libc::_exit(rc) };
    }

    // ── Parent: wait for ready, SIGKILL, verify ───────────────────────
    let deadline = std::time::Instant::now() + CHILD_READY_TIMEOUT;
    loop {
        if ready_path.exists() {
            break;
        }
        if std::time::Instant::now() > deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            panic!(
                "child did not signal ready within {:?} - pre-crash setup likely failed",
                CHILD_READY_TIMEOUT
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // SIGKILL the child: it MUST NOT reach set_arena_with_root.
    let kill_rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(
        kill_rc,
        0,
        "kill(SIGKILL) failed: errno {}",
        std::io::Error::last_os_error()
    );

    let mut status: libc::c_int = 0;
    let wait_rc = unsafe { libc::waitpid(pid, &mut status as *mut _, 0) };
    assert_eq!(
        wait_rc,
        pid,
        "waitpid returned {} (errno {}), expected child pid {}",
        wait_rc,
        std::io::Error::last_os_error(),
        pid
    );

    if !libc::WIFSIGNALED(status) {
        let exit_code = libc::WEXITSTATUS(status);
        panic!(
            "child exited normally with code {exit_code} - expected SIGKILL. \
             The test may no longer be exercising the crash window."
        );
    }
    let term_sig = libc::WTERMSIG(status);
    assert_eq!(
        term_sig,
        libc::SIGKILL,
        "child terminated by signal {term_sig}, expected SIGKILL"
    );

    // ── Verify per flavor ─────────────────────────────────────────────
    match flavor {
        CrashFlavor::SameSizeReAdvertise => {
            // File geometry unchanged; hash-verify addressed bytes.
            assert_reader_sees_root_at_init_geometry(&arena_path, &ctrl_path, &r0, &v0);
        }
        CrashFlavor::GrowAndReAdvertise => {
            // File grew; verify root + size ordering. Byte contents at
            // the pre-grow buffer offset are ALSO checked — grow must
            // not overwrite the addressed bytes.
            assert_reader_after_grow(&arena_path, &ctrl_path, &r0, &v0);
        }
    }
}

/// Child-side execution. Returns an exit code on failure; on success
/// this function never returns (SIGKILL kills it).
///
/// Both flavors preserve the invariant that `set_arena_with_root` NEVER
/// runs — the crash window opens strictly before final publish.
fn child_perform_pre_crash_setup(
    arena_path: &Path,
    ctrl_path: &Path,
    ready_path: &Path,
    flavor: CrashFlavor,
) -> i32 {
    match flavor {
        CrashFlavor::SameSizeReAdvertise => {
            // Just re-advertise the existing size. No file grow.
            let ctrl_result = Controller::open_or_create(ctrl_path);
            if ctrl_result.is_err() {
                return 20;
            }
            let mut ctrl = ctrl_result.unwrap();
            if ctrl
                .set_arena(&arena_path.to_string_lossy(), INIT_ARENA_SIZE)
                .is_err()
            {
                return 21;
            }
            drop(ctrl);
        }
        CrashFlavor::GrowAndReAdvertise => {
            // Step 1: grow the file.
            let mmap_result = create_arena(arena_path, GROWN_ARENA_SIZE);
            if mmap_result.is_err() {
                return 30;
            }
            drop(mmap_result.unwrap()); // flush + unmap before size advertise

            // Step 2: advertise larger size. Preserves current_root.
            let ctrl_result = Controller::open_or_create(ctrl_path);
            if ctrl_result.is_err() {
                return 31;
            }
            let mut ctrl = ctrl_result.unwrap();
            if ctrl
                .set_arena(&arena_path.to_string_lossy(), GROWN_ARENA_SIZE)
                .is_err()
            {
                return 32;
            }
            drop(ctrl);
        }
    }

    // Signal ready. sync_all so the parent's exists() check sees a
    // fully-materialized file.
    let ready_result = std::fs::File::create(ready_path).and_then(|mut f| {
        f.write_all(b"ready")?;
        f.sync_all()
    });
    if ready_result.is_err() {
        return 40;
    }

    // Block. Parent's SIGKILL terminates. If pause() returns
    // unexpectedly, exit with the sentinel so waitpid diagnoses.
    loop {
        unsafe { libc::pause() };
        return CHILD_UNEXPECTED_EXIT_CODE;
    }
}

/// Reader-side verification at the ORIGINAL (INIT_ARENA_SIZE) file
/// geometry. Opens a fresh Controller (mirrors a separate reader
/// process), reads `current_root`, mmaps the arena, and asserts:
///
/// - `current_root == expected_root`
/// - `arena_size == INIT_ARENA_SIZE` (unchanged)
/// - file size == INIT_ARENA_SIZE (unchanged)
/// - σ(bytes at active buffer) == `expected_root` (verify-on-read
///   succeeds)
fn assert_reader_sees_root_at_init_geometry(
    arena_path: &Path,
    ctrl_path: &Path,
    expected_root: &Hash,
    expected_bytes: &[u8],
) {
    let ctrl = Controller::open_or_create(ctrl_path).expect("reader open ctrl");
    let observed_root = Hash::from_bytes(ctrl.current_root());
    assert_eq!(
        &observed_root, expected_root,
        "F1 same-size flavor: reader saw current_root = {}, expected prior root = {}. \
         Writer advanced the root before the final publish point - (IM) broken.",
        observed_root, expected_root,
    );

    assert_eq!(
        ctrl.arena_size(),
        INIT_ARENA_SIZE,
        "F1 same-size flavor: arena_size unexpectedly changed - flavor invariant violated"
    );

    let file_size = std::fs::metadata(arena_path).expect("stat arena").len();
    assert_eq!(
        file_size, INIT_ARENA_SIZE,
        "F1 same-size flavor: file size changed from initial"
    );

    // Verify-on-read at the INIT geometry.
    let bytes = std::fs::read(arena_path).expect("read arena file");
    let header: ArenaHeader = *bytemuck::from_bytes(&bytes[..std::mem::size_of::<ArenaHeader>()]);
    let buf_offset = header
        .active_buffer_offset(file_size)
        .expect("valid header");
    let data_len = header.data_size as usize;
    let data = &bytes[buf_offset as usize..buf_offset as usize + data_len];
    let computed_root = data.hash();
    assert_eq!(
        &computed_root, expected_root,
        "F1 same-size flavor: σ(active_buffer_bytes) = {}, expected root = {}. \
         Torn read - the bytes at the addressed root don't hash to the root itself.",
        computed_root, expected_root,
    );
    assert_eq!(
        data, expected_bytes,
        "F1 same-size flavor: active buffer content drifted from V0 - torn write visible."
    );
}

/// Reader-side verification for the grow-and-advertise flavor. A fresh
/// reader arriving mid-grow can't hash-verify addressed bytes because
/// buffer_size is derived from the post-grow file_size (which puts the
/// active-buffer offset past where V0 was written at INIT geometry).
/// The substrate protects such readers via verify-on-read — mismatch
/// → refusal. F1's LOAD-BEARING claim at this flavor is:
///
/// - `current_root == R0` (root not prematurely advanced)
/// - `file_size >= arena_size` (grow-before-advertise ordering)
/// - `arena_size == GROWN_ARENA_SIZE` (writer's set_arena completed)
/// - Bytes at the ORIGINAL (pre-grow) buffer offset still equal `V0`
///   (the grow didn't overwrite pre-existing data — an old-geometry
///   polling reader still sees V0).
fn assert_reader_after_grow(
    arena_path: &Path,
    ctrl_path: &Path,
    expected_root: &Hash,
    expected_bytes: &[u8],
) {
    let ctrl = Controller::open_or_create(ctrl_path).expect("reader open ctrl");
    let observed_root = Hash::from_bytes(ctrl.current_root());
    assert_eq!(
        &observed_root, expected_root,
        "F1 grow flavor: reader saw current_root = {}, expected prior root = {}. \
         Writer advanced the root before the final publish - (IM) broken.",
        observed_root, expected_root,
    );

    assert_eq!(
        ctrl.arena_size(),
        GROWN_ARENA_SIZE,
        "F1 grow flavor: advertised arena_size should be the grown value \
         (child's set_arena completed before crash)",
    );

    let file_size = std::fs::metadata(arena_path).expect("stat arena").len();
    assert!(
        file_size >= GROWN_ARENA_SIZE,
        "F1 grow flavor: file size ({}) < advertised size ({}). \
         Writer advertised before growing - a fresh reader trying to mmap \
         `arena_size` bytes would run past EOF.",
        file_size,
        GROWN_ARENA_SIZE,
    );

    // Bytes at the ORIGINAL (INIT-geometry) buffer offset still equal
    // V0. The grow appended zeros beyond the original file end; the
    // original bytes are untouched. Compute the pre-grow buffer offset
    // from INIT_ARENA_SIZE, not from the current file size.
    let bytes = std::fs::read(arena_path).expect("read arena file");
    let header: ArenaHeader = *bytemuck::from_bytes(&bytes[..std::mem::size_of::<ArenaHeader>()]);
    // active_buffer + data_size are STILL the pre-crash values (the
    // child only touched size + sync counter via set_arena).
    let pre_grow_buf_size = ArenaHeader::buffer_size(INIT_ARENA_SIZE);
    let pre_grow_offset =
        ArenaHeader::HEADER_SIZE + header.active_buffer as u64 * pre_grow_buf_size;
    let data_len = header.data_size as usize;
    let data = &bytes[pre_grow_offset as usize..pre_grow_offset as usize + data_len];
    assert_eq!(
        data, expected_bytes,
        "F1 grow flavor: bytes at pre-grow buffer offset drifted from V0 - \
         grow overwrote pre-existing data (violates (IM) for polling readers).",
    );
    let computed_root = data.hash();
    assert_eq!(
        &computed_root, expected_root,
        "F1 grow flavor: σ(pre-grow_active_buffer_bytes) = {}, expected root = {}. \
         Old-geometry reader sees a hash mismatch - (IM) broken for pre-existing readers.",
        computed_root, expected_root,
    );
}
