// The shipped-path benchmark's workloads (bead ley-line-open-39d3a4).
//
// One program, compiled twice: a static Linux guest whose /work is the
// daemon's store over the workspace share (worker -> socket -> daemon
// storefs), and a native host binary run in an APFS directory holding the
// same tree. Timing is taken here, around each workload; boot, corpus load
// and tool warm-up are outside the timers.
//
// Arguments: <tree> <git> <rg> <git-dir> <out>. With none (the guest's
// catalog entry passes none) the guest's defaults apply.
//
// Output: one `key value unit` line per measurement, written to <out> and
// printed.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::process::Command;
use std::time::{Duration, Instant};

const LATENCY_FILE: &str = "bench/latency.txt";
const LATENCY_OPS: u32 = 1000;
// Each file closed on the share is one accepted proposal, and each accepted
// proposal re-snapshots the whole store (bead ley-line-open-39d3a4's
// finding), so the storm is kept short.
const STORM_FILES: u32 = 20;
const WARM_RUNS: u32 = 3;

struct Tools {
    tree: String,
    git: String,
    rg: String,
    git_dir: String,
    out: String,
}

fn tools() -> Tools {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [tree, git, rg, git_dir, out] => Tools {
            tree: tree.clone(),
            git: git.clone(),
            rg: rg.clone(),
            git_dir: git_dir.clone(),
            out: out.clone(),
        },
        [] => Tools {
            tree: "/work".into(),
            git: "/usr/bin/git".into(),
            rg: "/usr/bin/rg".into(),
            git_dir: "/repo.git".into(),
            out: "/work/bench/result.txt".into(),
        },
        other => {
            eprintln!("usage: bench <tree> <git> <rg> <git-dir> <out>; got {other:?}");
            std::process::exit(2);
        }
    }
}

fn run(program: &str, args: &[&str], cwd: &str) -> bool {
    match Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("HOME", std::env::temp_dir())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
    {
        Ok(output) => output.status.success(),
        Err(error) => {
            eprintln!("spawn {program}: {error}");
            false
        }
    }
}

fn timed(program: &str, args: &[&str], cwd: &str) -> (Duration, bool) {
    let start = Instant::now();
    let ok = run(program, args, cwd);
    (start.elapsed(), ok)
}

fn main() {
    let t = tools();
    let mut lines: Vec<String> = Vec::new();
    let mut line = |key: &str, value: f64, unit: &str| {
        let text = format!("{key} {value:.3} {unit}");
        println!("{text}");
        lines.push(text);
    };

    // Warm the tool binaries outside the tree.
    let tmp = std::env::temp_dir().display().to_string();
    run(&t.git, &["--version"], &tmp);
    run(&t.rg, &["--version"], &tmp);

    // Per-request latency: OPEN reaches the store at every open; a LOOKUP of
    // a name never seen before reaches it too.
    let latency = format!("{}/{LATENCY_FILE}", t.tree);
    let mut buf = Vec::with_capacity(8192);
    let start = Instant::now();
    for _ in 0..LATENCY_OPS {
        buf.clear();
        File::open(&latency)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .expect("open/read latency file");
    }
    line(
        "open_read_close",
        start.elapsed().as_secs_f64() * 1e6 / f64::from(LATENCY_OPS),
        "us/op",
    );
    let start = Instant::now();
    for i in 0..LATENCY_OPS {
        let missing = format!("{}/bench/missing-{i}", t.tree);
        assert!(fs::metadata(&missing).is_err(), "{missing} exists");
    }
    line(
        "lookup_miss",
        start.elapsed().as_secs_f64() * 1e6 / f64::from(LATENCY_OPS),
        "us/op",
    );

    // git status: the first run rehashes (the index was written elsewhere),
    // later runs compare stat only.
    let git_dir = format!("--git-dir={}", t.git_dir);
    let work_tree = format!("--work-tree={}", t.tree);
    let status = [
        "-c",
        "safe.directory=*",
        git_dir.as_str(),
        work_tree.as_str(),
        "status",
        "--porcelain",
    ];
    let (cold, ok) = timed(&t.git, &status, &t.tree);
    assert!(ok, "git status failed");
    line("git_status_cold", cold.as_secs_f64() * 1e3, "ms");
    let warm = (0..WARM_RUNS)
        .map(|_| timed(&t.git, &status, &t.tree).0)
        .min()
        .unwrap();
    line("git_status_warm_min", warm.as_secs_f64() * 1e3, "ms");

    // rg over the whole tree.
    let rg = ["-c", "--no-messages", "fn", "."];
    let (first, _) = timed(&t.rg, &rg, &t.tree);
    line("rg_first", first.as_secs_f64() * 1e3, "ms");
    let warm = (0..WARM_RUNS)
        .map(|_| timed(&t.rg, &rg, &t.tree).0)
        .min()
        .unwrap();
    line("rg_warm_min", warm.as_secs_f64() * 1e3, "ms");

    // Small-file storm: create, write, close. On the share every close is a
    // proposal that lands in the store and the checkout.
    let storm = format!("{}/bench/storm", t.tree);
    fs::create_dir_all(&storm).expect("storm directory");
    let start = Instant::now();
    for i in 0..STORM_FILES {
        let mut file = File::create(format!("{storm}/f{i}.txt")).expect("create");
        file.write_all(format!("small file {i}\n").as_bytes())
            .expect("write");
        drop(file);
    }
    let elapsed = start.elapsed();
    line("storm_total", elapsed.as_secs_f64() * 1e3, "ms");
    line(
        "storm_per_file",
        elapsed.as_secs_f64() * 1e3 / f64::from(STORM_FILES),
        "ms",
    );

    let mut report = lines.join("\n");
    report.push('\n');
    let mut out = File::create(&t.out).expect("result file");
    out.write_all(report.as_bytes()).expect("write result");
}
