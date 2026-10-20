// The agent in the guest (ADR-0044 phase C, bead ley-line-open-f210c2).
//
// Runs as the workload of an execution/v1 run, in /work: the store-backed
// virtio-fs share the guest init mounted. It uses file syscalls and two
// ordinary tools (git, rg), nothing LLO-specific. It talks to the host
// test only through files in the store: it writes `sync/guest.md` (each
// write a proposal the host reads over the socket) and waits on
// `sync/host.md` (each open reads the current blob).
//
// Each scenario's outcome is a line `PASS (x) ...` or `FAIL (x) ...` naming
// the syscall and its errno; the lines are written to `report.md` at the
// end. `mod seed` is prepended by the host.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};

extern "C" {
    fn close(fd: i32) -> i32;
}

const ESTALE: i32 = 116;

#[derive(Default)]
struct Report(Vec<String>);

impl Report {
    fn pass(&mut self, scenario: &str, what: String) {
        self.line(format!("PASS ({scenario}) {what}"));
    }
    fn fail(&mut self, scenario: &str, what: String) {
        self.line(format!("FAIL ({scenario}) {what}"));
    }
    fn check(&mut self, scenario: &str, ok: bool, what: String) {
        if ok {
            self.pass(scenario, what)
        } else {
            self.fail(scenario, what)
        }
    }
    fn line(&mut self, line: String) {
        println!("{line}");
        self.0.push(line);
    }
}

fn errno_of(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(-1)
}

/// close(2), reporting its errno: Rust's `File` drop discards it.
fn close_file(file: File) -> Result<(), i32> {
    let fd = file.into_raw_fd();
    // SAFETY: `fd` is an open descriptor this function now owns.
    if unsafe { close(fd) } == 0 {
        Ok(())
    } else {
        Err(errno_of(&std::io::Error::last_os_error()))
    }
}

fn show(result: Result<(), i32>) -> String {
    match result {
        Ok(()) => "0".into(),
        Err(errno) => format!("-1 errno={errno}"),
    }
}

/// open(O_WRONLY|O_CREAT|O_TRUNC), write, close: what `echo > path` does.
fn save(path: &str, text: &str) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| format!("open {path}: errno={}", errno_of(&e)))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("write {path}: errno={}", errno_of(&e)))?;
    close_file(file).map_err(|errno| format!("close {path}: errno={errno}"))
}

/// Replace an open file's bytes with `text`: seek to the start, truncate,
/// write (what an editor holding the file open does on save).
fn rewrite(file: &mut File, text: &str) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    file.write_all(text.as_bytes())
}

fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("read {path}: errno={}", errno_of(&e)))
}

/// Tell the host this step is done: `st-<step>` lands in the store.
fn signal(step: char) {
    if let Err(error) = save(seed::GUEST_SYNC, &format!("st-{step}\n")) {
        println!("FAIL signal {step}: {error}");
        std::process::exit(2);
    }
}

/// Wait for the host's `go-<step>`, reopening the file each time.
fn wait(step: char) {
    let want = format!("go-{step}\n");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if read(seed::HOST_SYNC).as_deref() == Ok(want.as_str()) {
            return;
        }
        if Instant::now() > deadline {
            println!("FAIL waiting for {want:?}");
            std::process::exit(3);
        }
        std::hint::spin_loop();
    }
}

fn run(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program)
        .args(args)
        .env("HOME", "/tmp")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
    {
        Ok(output) => (
            output.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ),
        Err(error) => (false, format!("spawn {program}: {error}")),
    }
}

fn main() {
    let mut r = Report::default();
    let cwd = std::env::current_dir().map(|d| d.display().to_string());
    r.check(
        "setup",
        cwd.as_deref().ok() == Some("/work"),
        format!("the workload starts in {cwd:?}, mounted by init"),
    );

    // (a) A read is the stored bytes.
    match read("a.go") {
        Ok(text) => r.check(
            "a",
            text == seed::A,
            format!("read(2) a.go: {} bytes, the stored blob: {}", text.len(), text == seed::A),
        ),
        Err(error) => r.fail("a", error),
    }

    // (b) An edit through open/write/close lands.
    match save("a.go", seed::A_EDIT) {
        Ok(()) => r.pass("b", "open(O_TRUNC)/write/close a.go: close=0".into()),
        Err(error) => r.fail("b", error),
    }
    signal('b');
    wait('b');

    // (c) A socket proposal moves b.go under the guest's open handle,
    // changing other lines than the guest's edit: write(2) stages and
    // close(2) lands the three-way merge, which the next open reads
    // (ADR-0045 T5, T6).
    match OpenOptions::new().read(true).write(true).open("b.go") {
        Ok(mut file) => {
            let mut held = String::new();
            let _ = file.read_to_string(&mut held);
            signal('c');
            wait('c');
            let wrote = rewrite(&mut file, seed::B_GUEST_TOP);
            let closed = close_file(file);
            let after = read("b.go");
            r.check(
                "c",
                wrote.is_ok() && closed.is_ok() && after.as_deref() == Ok(seed::B_MERGED),
                format!(
                    "b.go moved under the open handle, other lines: write ok={}, close(2) = {}; \
                     the next read is the merge: {}",
                    wrote.is_ok(),
                    show(closed),
                    after.as_deref() == Ok(seed::B_MERGED)
                ),
            );
        }
        Err(e) => r.fail("c", format!("open b.go: errno={}", errno_of(&e))),
    }
    signal('C');
    wait('C');

    // (C) The host and the guest change the same line: write(2) stages,
    // fsync(2) and close(2) are ESTALE, and the host's bytes stay.
    match OpenOptions::new().read(true).write(true).open("b.go") {
        Ok(mut file) => {
            let mut held = String::new();
            let _ = file.read_to_string(&mut held);
            signal('x');
            wait('x');
            let wrote = rewrite(&mut file, seed::B_GUEST_SAME);
            let fsync = file.sync_all().map_err(|e| errno_of(&e));
            let closed = close_file(file);
            r.check(
                "C",
                wrote.is_ok() && fsync == Err(ESTALE) && closed == Err(ESTALE),
                format!(
                    "b.go, the same line as the host's proposal: write ok={}, fsync(2) = {}, \
                     close(2) = {} (ESTALE={ESTALE})",
                    wrote.is_ok(),
                    show(fsync),
                    show(closed)
                ),
            );
        }
        Err(e) => r.fail("C", format!("open b.go: errno={}", errno_of(&e))),
    }
    signal('X');
    wait('X');

    // (d) A half-written edit lands: fsync(2) and close(2) are 0, and the
    // store records a diagnostic for it (ADR-0045 T3).
    match OpenOptions::new().write(true).truncate(true).open("c.go") {
        Ok(mut file) => {
            let wrote = file.write_all(seed::C_BROKEN.as_bytes());
            let fsync = file.sync_all().map_err(|e| errno_of(&e));
            let closed = close_file(file);
            r.check(
                "d",
                wrote.is_ok() && fsync.is_ok() && closed.is_ok(),
                format!(
                    "half-written c.go: write ok={}, fsync(2) = {}, close(2) = {}",
                    wrote.is_ok(),
                    show(fsync),
                    show(closed)
                ),
            );
        }
        Err(e) => r.fail("d", format!("open c.go: errno={}", errno_of(&e))),
    }
    signal('d');
    wait('d');

    // (e) create, unlink, and a write-temp-then-rename save.
    match save("n.go", seed::N) {
        Ok(()) => r.pass("e", "creat n.go/write/close: close=0".into()),
        Err(error) => r.fail("e", error),
    }
    match fs::remove_file("d.go") {
        Ok(()) => r.pass("e", "unlink(2) d.go = 0".into()),
        Err(e) => r.fail("e", format!("unlink(2) d.go: errno={}", errno_of(&e))),
    }
    let renamed = save(seed::A_TEMP, seed::A_RENAMED)
        .and_then(|()| {
            fs::rename(seed::A_TEMP, "a.go")
                .map_err(|e| format!("rename(2) {} -> a.go: errno={}", seed::A_TEMP, errno_of(&e)))
        })
        .and_then(|()| read("a.go"));
    match renamed {
        Ok(text) => r.check(
            "e",
            text == seed::A_RENAMED,
            format!(
                "write {} + rename(2) -> a.go = 0; a.go reads back the saved bytes: {}",
                seed::A_TEMP,
                text == seed::A_RENAMED
            ),
        ),
        Err(error) => r.fail("e", error),
    }
    signal('e');
    wait('e');

    // (f) git and rg over /work.
    let (ok, status) = run(
        "/usr/bin/git",
        &[
            "-c",
            "safe.directory=*",
            "--git-dir=/repo.git",
            "--work-tree=/work",
            "status",
            "--porcelain",
        ],
    );
    let lines: Vec<&str> = status.lines().collect();
    let expected = [" M a.go", " M b.go", " D d.go", "?? n.go"];
    r.check(
        "f",
        ok && expected.iter().all(|line| lines.contains(line)),
        format!("git status --porcelain over /work: exit ok={ok}; {lines:?}"),
    );
    let (ok, found) = run("/usr/bin/rg", &["-n", "--no-heading", "func N", "."]);
    r.check(
        "f",
        ok && found.lines().any(|line| line.ends_with("n.go:3:func N() int { return 7 }")),
        format!("rg -n 'func N' .: exit ok={ok}; {:?}", found.trim_end()),
    );
    let (ok, files) = run("/usr/bin/rg", &["--files", "."]);
    let mut listed: Vec<&str> = files.lines().collect();
    listed.sort_unstable();
    r.check(
        "f",
        ok && listed.iter().any(|f| f.ends_with("n.go")) && !listed.iter().any(|f| f.ends_with("d.go")),
        format!("rg --files .: {listed:?}"),
    );

    // (g) Fresh at open, and the attribute timeout bounds a stale stat.
    let before = read("g.go");
    let cached = fs::metadata("g.go").map(|m| m.len());
    r.check(
        "g",
        before.as_deref() == Ok(seed::G) && cached.as_ref().ok() == Some(&(seed::G.len() as u64)),
        format!("read g.go = stored bytes; stat size = {cached:?}"),
    );
    // t.go is only ever stat'ed: its size turns over when the cached
    // attributes expire, and no later.
    let t_cached = fs::metadata("t.go").map(|m| m.len());
    signal('g');
    wait('g');
    let proposed_at = Instant::now();
    let t_stale = fs::metadata("t.go").map(|m| m.len());
    let stale = fs::metadata("g.go").map(|m| m.len());
    r.line(format!(
        "NOTE (g) stat(2) g.go right after the host's proposal: size {stale:?} (stored {}, now {})",
        seed::G.len(),
        seed::G_HOST.len()
    ));
    match read("g.go") {
        Ok(text) => r.check(
            "g",
            text == seed::G_HOST,
            format!(
                "open/read g.go after the host's proposal: the new bytes ({} bytes): {}",
                text.len(),
                text == seed::G_HOST
            ),
        ),
        Err(error) => r.fail("g", error),
    }
    let bound = Duration::from_millis(seed::SETTLED_MS);
    let turned = loop {
        match fs::metadata("t.go") {
            Ok(m) if m.len() == seed::T_HOST.len() as u64 => break Some(proposed_at.elapsed()),
            _ if proposed_at.elapsed() > bound + Duration::from_secs(2) => break None,
            _ => std::hint::spin_loop(),
        }
    };
    r.check(
        "g",
        t_cached.as_ref().ok() == Some(&(seed::T.len() as u64))
            && turned.is_some_and(|after| after <= bound + Duration::from_millis(250)),
        format!(
            "stat(2) t.go (never opened): {t_cached:?} before the host's proposal, {t_stale:?} \
             right after it, {} bytes after {turned:?}; settled timeout {bound:?}",
            seed::T_HOST.len()
        ),
    );

    // (m) git decides whether to rehash a file from its stat (size, mtime,
    // ctime, inode): each of two successive same-size edits is reported by
    // the git status after it, the index holding the stat of the previous
    // state each time.
    let git = |args: &[&str]| {
        let mut all = vec!["-c", "safe.directory=*", "--git-dir=/repo.git", "--work-tree=/work"];
        all.extend_from_slice(args);
        run("/usr/bin/git", &all)
    };
    let mtime = || fs::metadata("m.go").and_then(|m| m.modified()).map_err(|e| errno_of(&e));
    let m_status = |r: &mut Report, after: &str, want: &str| {
        let (ok, status) = git(&["status", "--porcelain", "--", "m.go"]);
        let status = status.trim_end().to_owned();
        r.check(
            "m",
            ok && status == want,
            format!(
                "git status --porcelain -- m.go {after}: {status:?}, want {want:?}; \
                 stat mtime {:?}",
                mtime()
            ),
        );
    };
    // The index takes m.go's stat here.
    m_status(&mut r, "before any edit", "");
    match save("m.go", seed::M_ONE) {
        Ok(()) => m_status(&mut r, "after the first same-size edit", " M m.go"),
        Err(error) => r.fail("m", error),
    }
    let (ok, added) = git(&["add", "m.go"]);
    r.check("m", ok, format!("git add m.go: exit ok={ok} {added:?}"));
    m_status(&mut r, "after git add (the index takes the edit's stat)", "M  m.go");
    match save("m.go", seed::M_TWO) {
        Ok(()) => m_status(&mut r, "after the second same-size edit", "MM m.go"),
        Err(error) => r.fail("m", error),
    }

    // (p) Every file the agent makes is the store's (ADR-0045 T1, T4): a
    // README edit, a file in no projected language, chmod +x, a symlink, an
    // empty directory, and a directory renamed with a file in it.
    let made = save("README.md", seed::README_EDIT)
        .and_then(|()| save("notes", seed::NOTES))
        .and_then(|()| save("run.sh", seed::RUN_SH))
        .and_then(|()| {
            fs::set_permissions("run.sh", fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod(2) run.sh: errno={}", errno_of(&e)))
        })
        .and_then(|()| {
            std::os::unix::fs::symlink("a.go", "link.go")
                .map_err(|e| format!("symlink(2) link.go: errno={}", errno_of(&e)))
        })
        .and_then(|()| {
            fs::create_dir("empty").map_err(|e| format!("mkdir(2) empty: errno={}", errno_of(&e)))
        })
        .and_then(|()| {
            fs::create_dir("pkg").map_err(|e| format!("mkdir(2) pkg: errno={}", errno_of(&e)))
        })
        .and_then(|()| save("pkg/x.go", seed::LIB_X))
        .and_then(|()| {
            fs::rename("pkg", "lib").map_err(|e| format!("rename(2) pkg -> lib: errno={}", errno_of(&e)))
        });
    match made {
        Ok(()) => {
            let executable = fs::metadata("run.sh").map(|m| m.permissions().mode() & 0o111);
            let link = fs::read_link("link.go").map(|p| p.display().to_string());
            let empty = fs::metadata("empty").map(|m| m.is_dir());
            let moved = read("lib/x.go");
            let gone = fs::metadata("pkg").is_err();
            r.check(
                "p",
                executable.as_ref().ok() == Some(&0o111)
                    && link.as_deref().ok() == Some("a.go")
                    && empty.as_ref().ok() == Some(&true)
                    && moved.as_deref() == Ok(seed::LIB_X)
                    && gone,
                format!(
                    "README.md, notes, chmod +x run.sh ({executable:?}), symlink link.go \
                     ({link:?}), mkdir empty ({empty:?}), rename pkg -> lib ({:?}, pkg gone: \
                     {gone}): all 0",
                    moved.as_deref() == Ok(seed::LIB_X)
                ),
            );
        }
        Err(error) => r.fail("p", error),
    }
    signal('p');
    wait('p');

    let text = format!("{}\n", r.0.join("\n"));
    if let Err(error) = save(seed::REPORT, &text) {
        println!("FAIL report: {error}");
    }
    signal('z');
}
