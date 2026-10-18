use fs2::{available_space, FileExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const SNAPSHOT: &str = "refs/squashfs-builder/snapshot";
const PLAN: &str = "plan.json";
const OUTPUT: &str = "lineage.squashfs";
const WORK: &str = "workdir";
const BUDGET: u64 = 100_000_000_000;
const RESERVE: u64 = 5_000_000_000;

#[derive(Deserialize)]
struct Plan {
    version: u32,
    projects: Vec<Project>,
}

#[derive(Deserialize)]
struct Project {
    path: String,
    url: String,
    revision: String,
    #[serde(default)]
    copyfiles: Vec<Placement>,
    #[serde(default)]
    linkfiles: Vec<Placement>,
}

#[derive(Deserialize)]
struct Placement {
    src: String,
    dest: String,
}

#[derive(Serialize, Deserialize)]
struct State {
    initial_free: u64,
    commits: Vec<String>,
}

fn safe_path(s: &str) -> Result<()> {
    if s.is_empty() || s.starts_with('/') || s.contains('\\')
        || s.chars().any(char::is_control)
        || s.split('/').any(|p| p.is_empty() || p == "." || p == ".."
            || p.eq_ignore_ascii_case(".git") || p.eq_ignore_ascii_case(".repo"))
        || s.split('/').next() == Some(".source-image")
    {
        return Err(format!("unsafe source path: {s:?}").into());
    }
    Ok(())
}

fn validate(plan: &Plan) -> Result<()> {
    if plan.version != 1 || plan.projects.is_empty() {
        return Err("unsupported or empty plan".into());
    }
    let mut paths = HashSet::new();
    let mut destinations = HashSet::new();
    for p in &plan.projects {
        safe_path(&p.path)?;
        if !paths.insert(&p.path) || p.url.is_empty()
            || p.url.starts_with('-') || p.url.starts_with("ext::")
            || p.revision.is_empty() || p.revision.starts_with('-')
        {
            return Err(format!("unsupported/duplicate project: {}", p.path).into());
        }
        for item in p.copyfiles.iter().chain(&p.linkfiles) {
            if item.src != "." { safe_path(item.src.trim_end_matches('/'))?; }
            safe_path(&item.dest)?;
            if item.src.chars().any(|c| "*?[".contains(c)) || !destinations.insert(&item.dest) {
                return Err(format!("unsupported/duplicate placement: {}", item.dest).into());
            }
        }
        if p.copyfiles.iter().any(|x| x.src == ".") {
            return Err(format!("copyfile source is a directory: {}", p.path).into());
        }
    }
    for dest in destinations {
        let prefix = format!("{dest}/");
        if paths.iter().any(|p| p.as_str() == dest.as_str() || p.starts_with(prefix.as_str())) {
            return Err(format!("placement hides a project: {dest}").into());
        }
    }
    Ok(())
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(path.parent().ok_or("no parent")?)?.sync_all()?;
    Ok(())
}

struct Guard {
    root: PathBuf,
    floor: u64,
}

impl Guard {
    fn check(&self) -> Result<()> {
        let free = available_space(&self.root)?;
        if free < self.floor {
            return Err(format!("disk safety stop: {free} bytes free; floor {}", self.floor).into());
        }
        Ok(())
    }

    fn run(&self, mut cmd: Command) -> Result<()> {
        self.check()?;
        let mut child = cmd.spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                self.check()?;
                return if status.success() { Ok(()) }
                    else { Err(format!("command failed ({status}): {cmd:?}").into()) };
            }
            if let Err(err) = self.check() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg(format!("--git-dir={}", dir.display()))
        .arg("-c").arg("core.hooksPath=/dev/null")
        .arg("-c").arg("gc.auto=0")
        .arg("-c").arg("protocol.ext.allow=never")
        .env("GIT_TERMINAL_PROMPT", "0");
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_LFS_SKIP_SMUDGE", "GIT_LFS_SKIP_DOWNLOAD_ERRORS"] {
        c.env_remove(key);
    }
    c
}

fn git_output(mut cmd: Command) -> Result<String> {
    let out = cmd.stderr(Stdio::null()).output()?;
    if !out.status.success() { return Err("git command failed".into()); }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn set_head(repo: &Path, sha: &str, guard: &Guard) -> Result<()> {
    let mut cmd = git(repo);
    cmd.args(["update-ref", "HEAD", sha]);
    guard.run(cmd)
}

fn fetched_commit(repo: &Path) -> Option<String> {
    let mut cmd = git(repo);
    cmd.args(["rev-parse", "--verify", &format!("{SNAPSHOT}^{{commit}}")]);
    git_output(cmd).ok()
}

fn fetch(p: &Project, work: &Path, guard: &Guard) -> Result<String> {
    let repo = work.join("active.git");
    let owner = work.join("active-project");
    if fs::read_to_string(&owner).ok().as_deref() != Some(p.path.as_str()) {
        remove_any(&repo)?;
        remove_any(&owner)?;
        guard.run({ let mut c = Command::new("git"); c.args(["init", "--bare", "--quiet"]).arg(&repo); c })?;
        guard.run({ let mut c = git(&repo); c.args(["remote", "add", "origin", &p.url]); c })?;
        guard.run({
            let mut c = git(&repo);
            c.arg("-c").arg(format!("core.hooksPath={}", repo.join("hooks").display()));
            c.args(["lfs", "install", "--local"]).stdout(Stdio::null());
            c
        })?;
        fs::write(&owner, &p.path)?;
    }
    if let Some(sha) = fetched_commit(&repo) {
        set_head(&repo, &sha, guard)?;
        return Ok(sha);
    }
    let spec = format!("+{}:{SNAPSHOT}", p.revision);
    let mut failure = None;
    for attempt in 1..=4 {
        let mut cmd = git(&repo);
        cmd.args(["fetch", "--quiet", "--depth=1", "--no-tags", "--no-recurse-submodules",
                  "--no-write-fetch-head", "origin", &spec]);
        match guard.run(cmd) {
            Ok(()) => { failure = None; break; }
            Err(err) => {
                guard.check()?;
                eprintln!("fetch attempt {attempt}/4 failed: {}", p.path);
                failure = Some(err);
            }
        }
    }
    if let Some(err) = failure { return Err(err); }
    let sha = fetched_commit(&repo).ok_or("fetched ref does not resolve to a commit")?;
    if oid(&p.revision) && sha != p.revision {
        return Err(format!("revision mismatch for {}", p.path).into());
    }
    set_head(&repo, &sha, guard)?;
    Ok(sha)
}

fn remove_any(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() { fs::remove_dir_all(path)?; }
        else { fs::remove_file(path)?; }
    }
    Ok(())
}

fn check_parents(root: &Path, relative: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = relative.split('/').collect();
    let mut path = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        path.push(part);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if !meta.is_dir() { return Err(format!("non-directory destination: {}", path.display()).into()); }
        } else { fs::create_dir(&path)?; }
    }
    Ok(root.join(relative))
}

fn relative_link(dest: &str, target: &str) -> String {
    let mut parts: Vec<&str> = dest.split('/').collect();
    parts.pop();
    let target_parts: Vec<&str> = target.split('/').collect();
    let common = parts.iter().zip(&target_parts).take_while(|(a, b)| a == b).count();
    let mut result = vec![".."; parts.len() - common];
    result.extend_from_slice(&target_parts[common..]);
    if result.is_empty() { ".".into() } else { result.join("/") }
}

fn stage_project(p: &Project, all: &[Project], work: &Path, guard: &Guard,
                 sha: &str, index: usize, plan_bytes: &[u8]) -> Result<PathBuf> {
    let stage = work.join("stage");
    remove_any(&stage)?;
    fs::create_dir(&stage)?;
    let dummy = format!("{}/dummy", p.path);
    let dest = check_parents(&stage, &dummy)?.parent().ok_or("bad project path")?.to_path_buf();
    fs::create_dir_all(&dest)?;
    let repo = work.join("active.git");
    let mut cmd = git(&repo);
    cmd.arg(format!("--work-tree={}", dest.display()))
        .args(["-c", "core.bare=false", "checkout", "--quiet", "--detach", "-f", sha]);
    guard.run(cmd)?;
    let prefix = format!("{}/", p.path);
    for child in all {
        if let Some(tail) = child.path.strip_prefix(&prefix) {
            remove_any(&dest.join(tail))?;
        }
    }
    for other in all {
        for item in other.copyfiles.iter().chain(&other.linkfiles) {
            if item.dest.starts_with(&prefix) && !p.copyfiles.iter().chain(&p.linkfiles)
                .any(|owned| owned.dest == item.dest)
            {
                let x = stage.join(&item.dest);
                if let Ok(meta) = fs::symlink_metadata(&x) {
                    if meta.is_dir() { return Err(format!("placement hides directory: {}", item.dest).into()); }
                    fs::remove_file(x)?;
                }
            }
        }
    }
    for item in &p.copyfiles {
        let source = dest.join(&item.src);
        if !fs::symlink_metadata(&source)?.is_file() {
            return Err(format!("copyfile source is not regular: {}", source.display()).into());
        }
        let to = check_parents(&stage, &item.dest)?;
        if source != to {
            if fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
                return Err(format!("copyfile destination is a directory: {}", to.display()).into());
            }
            remove_any(&to)?;
            fs::copy(&source, &to)?;
        }
        let mode = fs::metadata(&to)?.permissions().mode() & !0o222;
        fs::set_permissions(to, fs::Permissions::from_mode(mode))?;
    }
    for item in &p.linkfiles {
        let src = item.src.trim_end_matches('/');
        let origin = if src == "." { dest.clone() } else { dest.join(src) };
        fs::symlink_metadata(&origin)?;
        let to = check_parents(&stage, &item.dest)?;
        if fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            return Err(format!("linkfile destination is a directory: {}", to.display()).into());
        }
        remove_any(&to)?;
        let target = if src == "." { p.path.clone() } else { format!("{}/{}", p.path, src) };
        std::os::unix::fs::symlink(relative_link(&item.dest, &target), to)?;
    }
    let dir = stage.join(".source-image/commits");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{index:06}")), format!("{sha}\n"))?;
    if index == 0 { fs::write(stage.join(".source-image/plan.json"), plan_bytes)?; }
    guard.check()?;
    Ok(stage)
}

fn checkpoint(stage: &Path, work: &Path, guard: &Guard, index: usize) -> Result<()> {
    let chunks = work.join("chunks");
    fs::create_dir_all(&chunks)?;
    let complete = chunks.join(format!("{index:06}.tar.xz"));
    let tmp = chunks.join(format!("{index:06}.tmp"));
    remove_any(&tmp)?;
    let mut cmd = Command::new("tar");
    cmd.args(["-b", "1", "-cJf"]).arg(&tmp).arg("-C").arg(stage).arg(".")
        .env("XZ_OPT", "-1 -T1");
    guard.run(cmd)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &complete)?;
    File::open(&chunks)?.sync_all()?;
    Ok(())
}

fn stream_chunk(archive: &Path, sink: &mut impl Write, guard: &Guard) -> Result<()> {
    let mut child = Command::new("xz").arg("-dc").arg(archive)
        .stdout(Stdio::piped()).spawn()?;
    let result: Result<()> = (|| {
        let mut buf = [0u8; 64 * 1024];
        let mut tail = Vec::with_capacity(buf.len() + 1024);
        loop {
            let n = child.stdout.as_mut().ok_or("xz output unavailable")?.read(&mut buf)?;
            if n == 0 { break; }
            guard.check()?;
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 1024 {
                let ready = tail.len() - 1024;
                sink.write_all(&tail[..ready])?;
                tail.drain(..ready);
            }
        }
        if tail.len() != 1024 || tail.iter().any(|x| *x != 0) {
            return Err(format!("invalid tar footer: {}", archive.display()).into());
        }
        Ok(())
    })();
    if result.is_err() { let _ = child.kill(); }
    let status = child.wait()?;
    result?;
    if !status.success() { return Err(format!("xz failed for {}: {status}", archive.display()).into()); }
    Ok(())
}

fn squashfs(partial: &Path, work: &Path, guard: &Guard, count: usize) -> Result<()> {
    remove_any(partial)?;
    guard.check()?;
    let mut cmd = Command::new("mksquashfs");
    cmd.arg("-").arg(partial).args(["-tar", "-comp", "xz", "-b", "1M",
        "-tailends", "-no-progress", "-exit-on-error", "-all-root",
        "-processors", "8", "-noappend", "-Xdict-size", "100%"]);
    let log = work.join("mksquashfs.log");
    let mut child = cmd.stdin(Stdio::piped()).stderr(File::create(&log)?).spawn()?;
    let result: Result<()> = (|| {
        let mut sink = child.stdin.take().ok_or("mksquashfs stdin unavailable")?;
        for i in 0..count {
            stream_chunk(&work.join(format!("chunks/{i:06}.tar.xz")), &mut sink, guard)?;
        }
        sink.write_all(&[0; 1024])?;
        Ok(())
    })();
    if result.is_err() { let _ = child.kill(); }
    let status = child.wait()?;
    let messages = fs::read(&log)?;
    for line in String::from_utf8_lossy(&messages).lines() {
        if !status.success() || !(line.ends_with(" already exists in the tar file, ignoring!")
            || line == "Empty tar filename after skipping leading /, ./, or ../, ignoring") {
            eprintln!("{line}");
        }
    }
    result?;
    if !status.success() { return Err(format!("mksquashfs failed: {status}").into()); }
    guard.check()?;
    File::open(partial)?.sync_all()?;
    Ok(())
}

fn marker(image: &Path, index: usize) -> Option<String> {
    let output = Command::new("unsquashfs").arg("-cat").arg(image)
        .arg(format!(".source-image/commits/{index:06}"))
        .stderr(Stdio::null()).output().ok()?;
    if !output.status.success() { return None; }
    let commit = std::str::from_utf8(&output.stdout).ok()?.trim();
    if oid(commit) { Some(commit.to_string()) } else { None }
}

fn build() -> Result<()> {
    let plan_bytes = fs::read(PLAN)?;
    let plan: Plan = serde_json::from_slice(&plan_bytes)?;
    validate(&plan)?;
    fs::create_dir_all(WORK)?;
    let work = fs::canonicalize(WORK)?;
    let root = fs::canonicalize(".")?;
    let output = root.join(OUTPUT);
    let lock = OpenOptions::new().create(true).read(true).write(true).open(work.join(".lock"))?;
    lock.try_lock_exclusive()?;
    let saved_plan = work.join(PLAN);
    let state_path = work.join("state.json");
    if saved_plan.exists() {
        if fs::read(&saved_plan)? != plan_bytes { return Err("work directory belongs to another plan".into()); }
    } else {
        if fs::read_dir(&work)?.filter_map(|e| e.ok())
            .any(|e| e.file_name().as_os_str() != std::ffi::OsStr::new(".lock")) {
            return Err("work directory is not empty".into());
        }
        fs::write(&saved_plan, &plan_bytes)?;
    }
    let mut state: State = if state_path.exists() {
        serde_json::from_slice(&fs::read(&state_path)?)?
    } else {
        let state = State { initial_free: available_space(&root)?, commits: Vec::new() };
        save_json(&state_path, &state)?;
        state
    };
    if state.commits.len() > plan.projects.len() { return Err("invalid checkpoint count".into()); }
    let guard = Guard { root, floor: RESERVE.max(
        state.initial_free.saturating_sub(BUDGET - RESERVE)) };
    let partial = output.with_extension("squashfs.partial");
    if output.exists() {
        if state.commits.len() != plan.projects.len() {
            return Err("final output already exists".into());
        }
        if partial.exists() {
            let a = fs::metadata(&output)?;
            let b = fs::metadata(&partial)?;
            if a.ino() != b.ino() || a.dev() != b.dev() {
                return Err("final output and partial differ".into());
            }
            fs::remove_file(&partial)?;
        }
        eprintln!("already complete: {}", output.display());
        return Ok(());
    }
    for i in 0..state.commits.len() {
        if !work.join(format!("chunks/{i:06}.tar.xz")).is_file() {
            return Err(format!("missing checkpoint {i}").into());
        }
    }
    guard.check()?;
    for (i, p) in plan.projects.iter().enumerate().skip(state.commits.len()) {
        eprintln!("[{}/{}] {}", i + 1, plan.projects.len(), p.path);
        let sha = fetch(p, &work, &guard)?;
        let stage = stage_project(p, &plan.projects, &work, &guard, &sha, i, &plan_bytes)?;
        checkpoint(&stage, &work, &guard, i)?;
        state.commits.push(sha);
        save_json(&state_path, &state)?;
        remove_any(&stage)?;
        remove_any(&work.join("active.git"))?;
        remove_any(&work.join("active-project"))?;
    }
    eprintln!("assembling {} projects", plan.projects.len());
    squashfs(&partial, &work, &guard, plan.projects.len())?;
    if marker(&partial, 0).as_deref() != state.commits.first().map(String::as_str)
        || marker(&partial, plan.projects.len() - 1).as_deref() != state.commits.last().map(String::as_str) {
        return Err("SquashFS commit markers missing".into());
    }
    guard.check()?;
    fs::hard_link(&partial, &output)?;
    fs::remove_file(partial)?;
    File::open(output.parent().ok_or("bad output directory")?)?.sync_all()?;
    eprintln!("COMPLETE: {}", output.display());
    Ok(())
}

fn main() {
    if std::env::args_os().len() != 1 { eprintln!("ERROR: unexpected arguments"); std::process::exit(1); }
    if let Err(err) = build() { eprintln!("ERROR: {err}"); std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_project() {
        let work = std::env::temp_dir().join(format!("squashfs-empty-{}", std::process::id()));
        fs::create_dir(&work).unwrap();
        let guard = Guard { root: work.clone(), floor: 0 };
        let repo = work.join("active.git");
        let mut init = Command::new("git");
        init.args(["init", "--bare", "--quiet"]).arg(&repo);
        guard.run(init).unwrap();
        let mut tree = git(&repo);
        tree.arg("mktree").stdin(Stdio::null());
        let tree = git_output(tree).unwrap();
        let mut commit = git(&repo);
        commit.args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                     "commit-tree", &tree, "-m", "empty"]);
        let sha = git_output(commit).unwrap();
        set_head(&repo, &sha, &guard).unwrap();
        let project = Project { path: "empty".into(), url: "unused".into(),
            revision: sha.clone(), copyfiles: vec![], linkfiles: vec![] };
        let stage = stage_project(&project, std::slice::from_ref(&project), &work,
                                  &guard, &sha, 0, b"{}").unwrap();
        assert!(stage.join("empty").is_dir());
        assert_eq!(fs::read_to_string(stage.join(".source-image/commits/000000")).unwrap(),
                   format!("{sha}\n"));
        fs::remove_dir_all(work).unwrap();
    }
    #[test]
    fn path_validation() {
        for bad in ["", "../foo", "a//b", "a/./b", "/abs", "a/.repo/b", "a/.git/b", ".source-image/x"] {
            assert!(safe_path(bad).is_err(), "{bad}");
        }
        assert!(safe_path("build/soong").is_ok());
    }
    #[test]
    fn relative_symlinks() {
        assert_eq!(relative_link("build/core", "build/make/core"), "make/core");
        assert_eq!(relative_link("link", "build/make/data"), "build/make/data");
    }
}
