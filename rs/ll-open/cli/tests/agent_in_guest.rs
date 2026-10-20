//! ADR-0044 phase C (bead `ley-line-open-f210c2`): an agent works through a
//! filesystem, with no LLO tool, inside a libkrun guest whose `/work` is the
//! daemon's store.
//!
//! The host runs the real `leyline execution-daemon --backend micro-vm
//! --source <checkout>`; the test starts an execution/v1 run whose RunGrant
//! carries a WorkspaceGrant over the socket, exactly as a caller would. The
//! daemon's libkrun backend spawns the real `leyline-krun-worker`, which
//! attaches the share; the guest init mounts it at `/work` and starts the
//! guest program (`agent_in_guest/guest.rs`) there. The guest and this test
//! coordinate through files in the store (the guest writes
//! `sync/guest.md`; the test proposes `sync/host.md` over the socket), so
//! every coordination step is itself a share write or a socket proposal.
//!
//! Scenarios (guest syscall + errno, then host verification over the
//! socket and in the checkout directory):
//!
//! - (a) a read is the stored bytes;
//! - (b) an open/write/close edit lands in the store and the checkout;
//! - (c) a socket proposal to a file the guest holds open, other lines than
//!   the guest's edit: write(2) stages, close(2) lands the three-way merge
//!   (ADR-0045 T5, T6);
//! - (C) the same line changed by both: fsync(2) and close(2) ESTALE, the
//!   host's bytes stay;
//! - (d) a half-written edit lands, with a `_parse_diagnostics` row
//!   (ADR-0045 T3);
//! - (e) create, unlink, and write-temp-then-rename land and reach the
//!   checkout;
//! - (f) `git status` and `rg` (Alpine aarch64 binaries in the guest rootfs,
//!   from `tools/guest_tools.sh`) over `/work`;
//! - (g) after a host proposal the guest reads the new bytes at its next
//!   open, and a cached `stat` turns over within the settled timeout;
//! - (m) two successive same-size edits of `m.go` are each reported by the
//!   `git status --porcelain` after it (` M`, then `MM` after `git add`):
//!   the share's file times move with each edit (bead
//!   `ley-line-open-b8100d`);
//! - (p) a README edit, a file in no projected language, `chmod +x`, a
//!   symlink, an empty directory and a directory rename persist in the
//!   store and the checkout (ADR-0045 T1, T4);
//! - (h) the daemon is killed with SIGKILL and restarted: the accepted
//!   guest edits persist in the store and the checkout.
//!
//! Every wait is a poll of the condition itself (a socket `read_source`, a
//! run status) with a deadline that fails only on a genuine hang; there is
//! no notification to wait on, and no sleep: the probe interval is the
//! round trip.
//!
//! Gated like the runtime's hypervisor tests: it runs where
//! `LEYLINE_LIBKRUNFW_PATH` names libkrunfw and reports a skip (not a pass)
//! elsewhere; `LEYLINE_REQUIRE_HYPERVISOR` turns the skip into a failure.
//! It also needs `LEYLINE_GUEST_TOOLS` (the rootfs fragment
//! `tools/guest_tools.sh` writes) and the worker binary
//! (`cargo build -p leyline-runtime --bin leyline-krun-worker`, or
//! `LEYLINE_KRUN_WORKER`).

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Instant;

use serde_json::json;

mod guest_harness;
#[path = "../../runtime/tests/hypervisor/mod.rs"]
mod hypervisor;
#[path = "agent_in_guest/seed.rs"]
mod seed;

use guest_harness::{Daemon, Host, STEP, call, git_dir_of, propose, stored};

const GUEST: &str = include_str!("agent_in_guest/guest.rs");
const SEED: &str = include_str!("agent_in_guest/seed.rs");

/// The daemon's diagnostic for `path` (ADR-0045 T3), read from its living
/// database: whether the file is projected, and the message.
fn diagnostic(host: &Host, path: &str) -> Option<(bool, String)> {
    let live = host.path("daemon.ctrl").with_extension("live.db");
    let conn =
        rusqlite::Connection::open_with_flags(&live, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT projected, message FROM _parse_diagnostics WHERE path = ?1",
        [path],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// (p) and (h): the mode, the symlink and the directories the guest made,
/// over the socket and in the checkout.
fn check_tree(host: &Host, daemon: &Daemon, results: &mut Results, scenario: &str) {
    let script = call(&daemon.sock, json!({"op": "read_source", "path": "run.sh"}));
    let executable = fs::metadata(host.checkout.join("run.sh"))
        .map(|m| m.permissions().mode() & 0o111)
        .ok();
    results.check(
        scenario,
        script["mode"] == "100755" && executable == Some(0o111),
        format!(
            "run.sh is 100755 in the store ({}) and executable on disk ({executable:?})",
            script["mode"]
        ),
    );
    let link = call(
        &daemon.sock,
        json!({"op": "read_source", "path": "link.go"}),
    );
    let on_disk = fs::read_link(host.checkout.join("link.go")).ok();
    results.check(
        scenario,
        link["mode"] == "120000"
            && link["content"] == "a.go"
            && on_disk.as_deref() == Some(Path::new("a.go")),
        format!("link.go is a symlink to a.go in the store ({link}) and on disk ({on_disk:?})"),
    );
    results.check(
        scenario,
        host.checkout.join("empty").is_dir()
            && host.checkout.join("lib").is_dir()
            && !host.checkout.join("pkg").exists(),
        "the empty directory and the renamed one are in the checkout; pkg is gone".into(),
    );
}

/// The guest rootfs: the guest program, the tools fragment, and a git
/// repository of the seed tree at `/repo.git`. Returns its content digest
/// and the agent binary's.
fn guest_rootfs(host: &Host, tools: &Path) -> (String, String) {
    let (staging, agent_digest) =
        guest_harness::stage_rootfs(host, tools, &format!("mod seed {{\n{SEED}\n}}\n{GUEST}"));
    // The repository `git status` in the guest compares /work against.
    let seed_tree = host.path("seed-tree");
    for (path, text) in seed::TREE {
        let file = seed_tree.join(path);
        fs::create_dir_all(file.parent().unwrap()).expect("seed directory");
        fs::write(file, text).expect("seed file");
    }
    git_dir_of(host, &seed_tree, &staging.join("repo.git"));
    (host.publish_rootfs(&staging), agent_digest)
}

/// Waits for the guest's `st-<step>` in the store, failing fast when the
/// run ends first.
fn await_guest(host: &Host, daemon: &Daemon, run_id: &str, step: char) {
    let want = format!("st-{step}\n");
    let deadline = Instant::now() + STEP;
    loop {
        if stored(&daemon.sock, seed::GUEST_SYNC).as_deref() == Some(want.as_str()) {
            return;
        }
        let status = call(
            &daemon.sock,
            json!({"op": "llo_execution_status", "runId": run_id}),
        );
        let state = status.to_string();
        assert!(
            Instant::now() < deadline && !state.contains("succeeded") && !state.contains("failed"),
            "the guest never reached step {step}: {status}\nevents: {}\nreport: {:?}\nlog:\n{}",
            call(
                &daemon.sock,
                json!({"op": "llo_execution_inspect", "runId": run_id, "afterSequence": 0}),
            ),
            stored(&daemon.sock, seed::REPORT),
            host.log("run")
        );
        std::hint::spin_loop();
    }
}

fn go(daemon: &Daemon, step: char) {
    propose(&daemon.sock, seed::HOST_SYNC, &format!("go-{step}\n"));
}

#[derive(Default)]
struct Results(Vec<String>);

impl Results {
    fn check(&mut self, scenario: &str, ok: bool, what: String) {
        self.0.push(format!(
            "{} ({scenario}) host: {what}",
            if ok { "PASS" } else { "FAIL" }
        ));
    }
}

#[test]
fn an_agent_in_a_microvm_works_through_the_store_backed_filesystem() {
    let Some(tools) = guest_harness::guest_tools_if_hypervisor() else {
        return;
    };
    let host = guest_harness::host(
        |dir| {
            let checkout = dir.join("checkout");
            for (path, text) in seed::TREE {
                let file = checkout.join(path);
                fs::create_dir_all(file.parent().unwrap()).expect("checkout directory");
                fs::write(file, text).expect("checkout file");
            }
            checkout
        },
        seed::SETTLED_MS,
        seed::CHANGED_MS,
    )
    .expect("libkrunfw was found a moment ago");
    let (rootfs, agent_digest) = guest_rootfs(&host, &tools);

    // Lifetime 1: learn the tree root the grant authorizes, with an empty
    // catalog.
    let graph_root = host.graph_root("a.go");
    host.write_catalog(&rootfs, &agent_digest, &graph_root);

    // Lifetime 2: the run.
    let mut results = Results::default();
    let daemon = host.daemon("run");
    let (_, root_now) = guest_harness::read_source(&daemon.sock, "a.go").expect("a.go");
    assert_eq!(
        format!("blake3-256:{root_now}"),
        graph_root,
        "a restart keeps the tree root"
    );
    let run_id = host.start_run(
        &daemon,
        "run",
        "agent-in-guest",
        &agent_digest,
        &graph_root,
        guest_harness::LIMITS,
    );

    // (b) the guest's edit is in the store and the checkout.
    await_guest(&host, &daemon, &run_id, 'b');
    results.check(
        "b",
        stored(&daemon.sock, "a.go").as_deref() == Some(seed::A_EDIT)
            && host.on_disk("a.go").as_deref() == Some(seed::A_EDIT),
        "read_source a.go and checkout a.go hold the guest's edit".into(),
    );
    go(&daemon, 'b');

    // (c) a socket proposal moves b.go under the guest's open handle; the
    // two changed different lines, so both land, merged (ADR-0045 T5).
    await_guest(&host, &daemon, &run_id, 'c');
    propose(&daemon.sock, "b.go", seed::B_HOST);
    go(&daemon, 'c');
    await_guest(&host, &daemon, &run_id, 'C');
    results.check(
        "c",
        stored(&daemon.sock, "b.go").as_deref() == Some(seed::B_MERGED)
            && host.on_disk("b.go").as_deref() == Some(seed::B_MERGED),
        "b.go holds the socket proposal and the guest's edit, merged, in the store and the \
         checkout"
            .into(),
    );
    go(&daemon, 'C');

    // (C) the same line, changed by both: the host's bytes stay.
    await_guest(&host, &daemon, &run_id, 'x');
    propose(&daemon.sock, "b.go", seed::B_HOST_AGAIN);
    go(&daemon, 'x');
    await_guest(&host, &daemon, &run_id, 'X');
    results.check(
        "C",
        stored(&daemon.sock, "b.go").as_deref() == Some(seed::B_HOST_AGAIN)
            && host.on_disk("b.go").as_deref() == Some(seed::B_HOST_AGAIN),
        "b.go holds the socket proposal; the guest's overlapping edit landed nowhere".into(),
    );
    go(&daemon, 'X');

    // (d) the half-written edit landed, with a diagnostic.
    await_guest(&host, &daemon, &run_id, 'd');
    let diagnostic = diagnostic(&host, "c.go");
    results.check(
        "d",
        stored(&daemon.sock, "c.go").as_deref() == Some(seed::C_BROKEN)
            && host.on_disk("c.go").as_deref() == Some(seed::C_BROKEN)
            && diagnostic.as_ref().is_some_and(|(projected, _)| *projected),
        format!(
            "c.go holds the half-written bytes in the store and the checkout; its diagnostic: \
             {diagnostic:?}"
        ),
    );
    go(&daemon, 'd');

    // (e) create, unlink, rename.
    await_guest(&host, &daemon, &run_id, 'e');
    results.check(
        "e",
        stored(&daemon.sock, "n.go").as_deref() == Some(seed::N)
            && host.on_disk("n.go").as_deref() == Some(seed::N),
        "n.go created in the store and the checkout".into(),
    );
    results.check(
        "e",
        stored(&daemon.sock, "d.go").is_none() && host.on_disk("d.go").is_none(),
        "d.go deleted from the store and the checkout".into(),
    );
    results.check(
        "e",
        stored(&daemon.sock, "a.go").as_deref() == Some(seed::A_RENAMED)
            && host.on_disk("a.go").as_deref() == Some(seed::A_RENAMED)
            && stored(&daemon.sock, seed::A_TEMP).is_none()
            && host.on_disk(seed::A_TEMP).is_none(),
        "a.go holds the renamed save; the temp file reached neither".into(),
    );
    go(&daemon, 'e');

    // (g) a host proposal the guest sees at its next open.
    await_guest(&host, &daemon, &run_id, 'g');
    propose(&daemon.sock, "g.go", seed::G_HOST);
    propose(&daemon.sock, "t.go", seed::T_HOST);
    go(&daemon, 'g');

    // (p) every file the guest made is in the store and the checkout.
    await_guest(&host, &daemon, &run_id, 'p');
    for (path, text) in [
        ("README.md", seed::README_EDIT),
        ("notes", seed::NOTES),
        ("run.sh", seed::RUN_SH),
        ("lib/x.go", seed::LIB_X),
    ] {
        results.check(
            "p",
            stored(&daemon.sock, path).as_deref() == Some(text)
                && host.on_disk(path).as_deref() == Some(text),
            format!("{path} is in the store and the checkout"),
        );
    }
    check_tree(&host, &daemon, &mut results, "p");
    go(&daemon, 'p');

    await_guest(&host, &daemon, &run_id, 'z');
    let terminal = host.await_end(&daemon, "run", &run_id, STEP);
    results.check(
        "run",
        terminal.to_string().contains("succeeded"),
        format!("the execution/v1 run ended: {terminal}"),
    );
    let report = stored(&daemon.sock, seed::REPORT).unwrap_or_default();
    results.check(
        "report",
        host.on_disk(seed::REPORT).as_deref() == Some(report.as_str()),
        "the guest's report.md is in the store and the checkout".into(),
    );

    // (h) SIGKILL the daemon, restart it: the accepted guest edits persist.
    drop(daemon);
    let daemon = host.daemon("restart");
    let expected: BTreeMap<&str, Option<&str>> = BTreeMap::from([
        ("a.go", Some(seed::A_RENAMED)),
        ("b.go", Some(seed::B_HOST_AGAIN)),
        ("c.go", Some(seed::C_BROKEN)),
        ("d.go", None),
        ("n.go", Some(seed::N)),
        ("g.go", Some(seed::G_HOST)),
        ("t.go", Some(seed::T_HOST)),
        ("m.go", Some(seed::M_TWO)),
        ("README.md", Some(seed::README_EDIT)),
        ("notes", Some(seed::NOTES)),
        ("lib/x.go", Some(seed::LIB_X)),
    ]);
    for (path, text) in &expected {
        results.check(
            "h",
            stored(&daemon.sock, path).as_deref() == *text
                && host.on_disk(path).as_deref() == *text,
            format!(
                "after kill -9 and restart, {path} is {}",
                text.map_or("absent", |_| "the accepted bytes")
            ),
        );
    }
    check_tree(&host, &daemon, &mut results, "h");
    drop(daemon);

    let mut lines: Vec<String> = report.lines().map(str::to_owned).collect();
    lines.extend(results.0);
    eprintln!("agent-in-guest results:\n{}", lines.join("\n"));
    let scenarios = ["a", "b", "c", "C", "d", "e", "f", "g", "m", "p", "h"];
    for scenario in scenarios {
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with(&format!("PASS ({scenario})"))),
            "scenario ({scenario}) has no PASS:\n{}",
            lines.join("\n")
        );
    }
    let failed: Vec<&String> = lines.iter().filter(|l| l.starts_with("FAIL")).collect();
    assert!(
        failed.is_empty(),
        "failures:\n{failed:#?}\nall:\n{}",
        lines.join("\n")
    );
}
