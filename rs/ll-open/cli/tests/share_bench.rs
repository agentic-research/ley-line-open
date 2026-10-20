//! The shipped workspace-share path, measured (bead `ley-line-open-39d3a4`):
//! a libkrun guest whose `/work` is the daemon's store, every FUSE request
//! travelling worker -> share socket -> daemon storefs, against the same
//! workloads in a plain APFS directory.
//!
//! Ignored by default (it boots a VM over a ~10k-file corpus and takes
//! minutes); run it with
//!
//! ```text
//! LEYLINE_LIBKRUNFW_PATH=... LEYLINE_GUEST_TOOLS=<dir>/rootfs \
//!   cargo test -p leyline-cli --test share_bench -- --ignored --nocapture
//! ```
//!
//! after `cargo build -p leyline-runtime --bin leyline-krun-worker`. The
//! workloads are `share_bench/bench.rs` (see its header). There is no host
//! NFS, fuse-t, macFUSE or FSKit leg: those drive host kernel filesystem
//! code, and a loopback NFS load kernel-panicked a development Mac (bead
//! `ley-line-open-2d49ce`).
//!
//! The corpus is this repository's tracked files plus crate sources from
//! `$CARGO_HOME/registry/src` until `LEYLINE_BENCH_FILES` files (default
//! 10000); symlinks, `.gitignore` files and files over 4 MiB are left out.
//! `LEYLINE_BENCH_OUT` names a file to write the result table to.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod guest_harness;
#[path = "../../runtime/tests/hypervisor/mod.rs"]
mod hypervisor;

use guest_harness::{Limits, git_dir_of, stored};

const BENCH: &str = include_str!("share_bench/bench.rs");
const RESULT: &str = "bench/result.txt";
const MAX_FILE: u64 = 4 << 20;
const LIMITS: Limits = Limits {
    wall_time_ms: 900_000,
    memory_bytes: 2048 << 20,
    cpu_millis: 4_000,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root")
}

fn keep(path: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_file() && meta.len() <= MAX_FILE && path.file_name().is_some_and(|n| n != ".gitignore")
}

/// The corpus: `(relative path, source file)` pairs.
fn corpus(want: usize) -> Vec<(String, PathBuf)> {
    let root = repo_root();
    let listed = Command::new("git")
        .current_dir(&root)
        .args(["ls-files", "-z"])
        .output()
        .expect("git ls-files");
    assert!(listed.status.success(), "git ls-files failed");
    let mut files: Vec<(String, PathBuf)> = listed
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok())
        .map(|p| (p.to_owned(), root.join(p)))
        .filter(|(_, source)| keep(source))
        .collect();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let registry = cargo_home.map(|c| c.join("registry/src"));
    let mut crates: Vec<PathBuf> = registry
        .iter()
        .flat_map(|r| fs::read_dir(r).into_iter().flatten())
        .flatten()
        .flat_map(|index| fs::read_dir(index.path()).into_iter().flatten())
        .flatten()
        .map(|c| c.path())
        .filter(|p| p.is_dir())
        .collect();
    crates.sort();
    'crates: for krate in crates {
        let name = krate.file_name().unwrap().to_string_lossy().into_owned();
        let mut stack = vec![krate.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<_> = fs::read_dir(&dir).into_iter().flatten().flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    stack.push(path);
                } else if keep(&path)
                    && let Ok(rel) = path.strip_prefix(&krate)
                    && let Some(rel) = rel.to_str()
                {
                    files.push((format!("vendor/{name}/{rel}"), path));
                }
            }
        }
        if files.len() >= want {
            break 'crates;
        }
    }
    files
}

fn write_tree(files: &[(String, PathBuf)], to: &Path) {
    for (rel, source) in files {
        let target = to.join(rel);
        fs::create_dir_all(target.parent().unwrap()).expect("corpus directory");
        fs::copy(source, &target).expect("copy corpus file");
    }
    let latency = to.join("bench/latency.txt");
    fs::create_dir_all(latency.parent().unwrap()).expect("bench directory");
    fs::write(latency, "x".repeat(4096)).expect("latency file");
}

fn which(tool: &str) -> String {
    let out = Command::new("/usr/bin/which")
        .arg(tool)
        .output()
        .expect("which");
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert!(!path.is_empty(), "{tool} is not on PATH");
    path
}

fn parse(result: &str) -> Vec<(String, String)> {
    result
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(2, ' ');
            Some((parts.next()?.to_owned(), parts.next()?.to_owned()))
        })
        .collect()
}

#[test]
#[ignore = "boots a VM over a 10k-file corpus; run with --ignored"]
fn the_shipped_share_path_against_apfs() {
    let Some(tools) = guest_harness::guest_tools_if_hypervisor() else {
        return;
    };
    let want: usize = std::env::var("LEYLINE_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let files = corpus(want);
    let mut host = guest_harness::host(
        |dir| {
            let checkout = dir.join("checkout");
            write_tree(&files, &checkout);
            checkout
        },
        3_000,
        500,
    )
    .expect("libkrunfw was found a moment ago");
    host.ready_within = Duration::from_secs(900);

    // The guest rootfs: the bench program, the tools, and /repo.git.
    let (staging, agent_digest) = guest_harness::stage_rootfs(&host, &tools, BENCH);
    let seed = host.path("seed-tree");
    write_tree(&files, &seed);
    git_dir_of(&host, &seed, &staging.join("repo.git"));
    let rootfs = host.publish_rootfs(&staging);

    // The APFS floor: the same tree and a git directory written elsewhere,
    // as the guest's is.
    let apfs = host.path("apfs");
    write_tree(&files, &apfs);
    let apfs_seed = host.path("apfs-seed");
    write_tree(&files, &apfs_seed);
    let apfs_git = host.path("apfs.git");
    git_dir_of(&host, &apfs_seed, &apfs_git);
    let native = host.path("bench-native");
    let source = host.path("bench.rs");
    fs::write(&source, BENCH).expect("bench source");
    let status = Command::new("rustc")
        .args(["--edition", "2021", "-o"])
        .arg(&native)
        .arg(&source)
        .status()
        .expect("rustc");
    assert!(status.success(), "native bench compilation failed");
    let apfs_out = host.path("apfs-result.txt");
    let ran = Command::new(&native)
        .arg(&apfs)
        .arg("/usr/bin/git")
        .arg(which("rg"))
        .arg(&apfs_git)
        .arg(&apfs_out)
        .status()
        .expect("run native bench");
    assert!(ran.success(), "native bench failed");
    let apfs_result = fs::read_to_string(&apfs_out).expect("apfs result");

    // The guest leg.
    let graph_root = host.graph_root("bench/latency.txt");
    host.write_catalog(&rootfs, &agent_digest, &graph_root);
    let daemon = host.daemon("run");
    let run_id = host.start_run(
        &daemon,
        "run",
        "share-bench",
        &agent_digest,
        &graph_root,
        LIMITS,
    );
    let terminal = host.await_end(&daemon, &run_id, Duration::from_millis(LIMITS.wall_time_ms));
    let guest_result = stored(&daemon.sock, RESULT).unwrap_or_default();
    assert!(
        terminal.to_string().contains("succeeded") && !guest_result.is_empty(),
        "the guest bench did not finish: {terminal}\nlog:\n{}",
        host.log("run")
    );

    let guest = parse(&guest_result);
    let floor = parse(&apfs_result);
    let mut table = format!(
        "share bench: {} files\n{:<22} {:>18} {:>18}\n",
        files.len(),
        "measurement",
        "guest (share)",
        "APFS"
    );
    for (key, value) in &guest {
        let apfs = floor
            .iter()
            .find(|(k, _)| k == key)
            .map_or("-", |(_, v)| v.as_str());
        table.push_str(&format!("{key:<22} {value:>18} {apfs:>18}\n"));
    }
    eprintln!("{table}");
    if let Some(out) = std::env::var_os("LEYLINE_BENCH_OUT") {
        fs::write(out, &table).expect("write bench table");
    }
}
