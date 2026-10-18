mod http;

use fs2::{available_space, FileExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
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
            || !(p.url.starts_with("https://github.com/") || p.url.starts_with("https://android.googlesource.com/"))
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
                    else { Err(format!("command failed ({status}): {}", cmd.get_program().to_string_lossy()).into()) };
            }
            if let Err(err) = self.check() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err);
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

fn oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
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
                 sha: &str, index: usize, plan_bytes: &[u8], raw: bool) -> Result<PathBuf> {
    let stage = work.join(if raw { "source" } else { "stage" });
    if !raw { remove_any(&stage)?; }
    fs::create_dir_all(&stage)?;
    let dest = check_parents(&stage, &p.path)?;
    remove_any(&dest)?;
    fs::create_dir(&dest)?;
    http::unpack(p, &work.join(format!("downloads/{index:06}")), &dest, sha, guard)?;
    let prefix = format!("{}/", p.path);
    for child in all {
        if let Some(tail) = child.path.strip_prefix(&prefix) {
            remove_any(&check_parents(&dest, tail)?)?;
        }
    }
    for other in all {
        for item in other.copyfiles.iter().chain(&other.linkfiles) {
            if item.dest.starts_with(&prefix) && !p.copyfiles.iter().chain(&p.linkfiles)
                .any(|owned| owned.dest == item.dest)
            {
                let x = check_parents(&stage, &item.dest)?;
                if let Ok(meta) = fs::symlink_metadata(&x) {
                    if meta.is_dir() { return Err(format!("placement hides directory: {}", item.dest).into()); }
                    fs::remove_file(x)?;
                }
            }
        }
    }
    if !raw { place_files(p, &stage)?; }
    let dir = stage.join(".source-image/commits");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{index:06}")), format!("{sha}\n"))?;
    if index == 0 { fs::write(stage.join(".source-image/plan.json"), plan_bytes)?; }
    guard.check()?;
    Ok(stage)
}

fn place_files(p: &Project, stage: &Path) -> Result<()> {
    let dest = stage.join(&p.path);
    for item in &p.copyfiles {
        let source = check_parents(&dest, &item.src)?;
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
        let origin = if src == "." { dest.clone() } else { check_parents(&dest, src)? };
        fs::symlink_metadata(&origin)?;
        let to = check_parents(&stage, &item.dest)?;
        if fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            return Err(format!("linkfile destination is a directory: {}", to.display()).into());
        }
        remove_any(&to)?;
        let target = if src == "." { p.path.clone() } else { format!("{}/{}", p.path, src) };
        std::os::unix::fs::symlink(relative_link(&item.dest, &target), to)?;
    }
    Ok(())
}

fn checkpoint(stage: &Path, work: &Path, guard: &Guard, index: usize) -> Result<()> {
    let chunks = work.join("chunks");
    fs::create_dir_all(&chunks)?;
    let complete = chunks.join(format!("{index:06}.tar.zst"));
    let tmp = chunks.join(format!("{index:06}.tmp"));
    remove_any(&tmp)?;
    let mut cmd = Command::new("tar");
    cmd.args(["-b", "128", "-c", "-I", "zstd -T0 -5", "-f"])
        .arg(&tmp).arg("-C").arg(stage).arg(".");
    guard.run(cmd)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, &complete)?;
    File::open(&chunks)?.sync_all()?;
    Ok(())
}

fn chunk_file(work: &Path, index: usize) -> PathBuf {
    let zst = work.join(format!("chunks/{index:06}.tar.zst"));
    if zst.is_file() { zst } else { work.join(format!("chunks/{index:06}.tar.xz")) }
}

fn stream_chunk(archive: &Path, sink: &mut impl Write, guard: &Guard) -> Result<()> {
    let codec = if archive.extension() == Some(std::ffi::OsStr::new("zst")) { "zstd" } else { "xz" };
    let mut child = Command::new(codec).arg("-dc").arg(archive)
        .stdout(Stdio::piped()).spawn()?;
    let result: Result<()> = (|| {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = child.stdout.as_mut().ok_or("decompress output unavailable")?.read(&mut buf)?;
            if n == 0 { break; }
            guard.check()?;
            sink.write_all(&buf[..n])?;
        }
        Ok(())
    })();
    if result.is_err() { let _ = child.kill(); }
    let status = child.wait()?;
    result?;
    if !status.success() { return Err(format!("{codec} failed for {}: {status}", archive.display()).into()); }
    Ok(())
}

fn squashfs(partial: &Path, work: &Path, guard: &Guard, count: usize) -> Result<()> {
    remove_any(partial)?;
    guard.check()?;
    let mut cmd = Command::new("sqfstar");
    cmd.args(["-ignore-zeros", "-comp", "xz", "-b", "1M", "-no-progress",
        "-exit-on-error", "-all-root", "-exports", "-Xdict-size", "100%"]).arg(partial);
    let log = work.join("sqfstar.log");
    let mut child = cmd.stdin(Stdio::piped()).stderr(File::create(&log)?).spawn()?;
    let result: Result<()> = (|| {
        let mut sink = child.stdin.take().ok_or("sqfstar stdin unavailable")?;
        for i in 0..count {
            stream_chunk(&chunk_file(work, i), &mut sink, guard)?;
        }
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
    if !status.success() { return Err(format!("sqfstar failed: {status}").into()); }
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

fn finish_checkpoint(state: &mut State, path: &Path, work: &Path,
                     worker: thread::ScopedJoinHandle<'_, Result<String>>) -> Result<()> {
    state.commits.push(worker.join().map_err(|_| "checkpoint worker panicked")??);
    save_json(path, state)?;
    remove_any(&work.join("stage"))?;
    remove_any(&work.join(format!("downloads/{:06}", state.commits.len() - 1)))?;
    Ok(())
}

fn build(raw: bool) -> Result<()> {
    let plan_bytes = fs::read(PLAN)?;
    let plan: Plan = serde_json::from_slice(&plan_bytes)?;
    validate(&plan)?;
    if raw && plan.projects.windows(2).any(|p| p[0].path >= p[1].path) {
        return Err("raw mode requires projects sorted by path".into());
    }
    let work = if raw { "raw-workdir" } else { WORK };
    fs::create_dir_all(work)?;
    let work = fs::canonicalize(work)?;
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
    let guard = Guard { root, floor: if raw { RESERVE } else { RESERVE.max(
        state.initial_free.saturating_sub(BUDGET - RESERVE)) } };
    let partial = output.with_extension("squashfs.partial");
    if !raw && output.exists() {
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
    for (i, sha) in state.commits.iter().enumerate() {
        if raw {
            let marker = work.join(format!("source/.source-image/commits/{i:06}"));
            if fs::read_to_string(marker)?.trim() != sha
                || !work.join("source").join(&plan.projects[i].path).is_dir() {
                return Err(format!("missing raw project {i}").into());
            }
        } else if !chunk_file(&work, i).is_file() {
            return Err(format!("missing checkpoint {i}").into());
        }
    }
    guard.check()?;
    thread::scope(|scope| -> Result<()> {
        let mut pending: Option<thread::ScopedJoinHandle<'_, Result<String>>> = None;
        let mut downloads = VecDeque::new();
        let mut next = state.commits.len();
        let work = &work;
        let guard = &guard;
        for (i, p) in plan.projects.iter().enumerate().skip(state.commits.len()) {
            while next < plan.projects.len() && downloads.len() < 16 {
                let project = &plan.projects[next];
                let dir = work.join(format!("downloads/{next:06}"));
                downloads.push_back(scope.spawn(move || http::fetch(project, &dir, guard)));
                next += 1;
            }
            eprintln!("[{}/{}] {}", i + 1, plan.projects.len(), p.path);
            if pending.as_ref().is_some_and(|worker| worker.is_finished()) {
                finish_checkpoint(&mut state, &state_path, work, pending.take().unwrap())?;
            }
            let fetched = downloads.pop_front().unwrap().join()
                .map_err(|_| "download worker panicked")?;
            if let Some(worker) = pending.take() {
                finish_checkpoint(&mut state, &state_path, work, worker)?;
            }
            let sha = fetched.map_err(|e| format!("{}: {e}", p.path))?;
            let stage = stage_project(p, &plan.projects, work, guard, &sha, i, &plan_bytes, raw)?;
            if raw {
                state.commits.push(sha);
                save_json(&state_path, &state)?;
                remove_any(&work.join(format!("downloads/{i:06}")))?;
            } else {
                pending = Some(scope.spawn(move || {
                    checkpoint(&stage, work, guard, i)?;
                    Ok(sha)
                }));
            }
        }
        if let Some(worker) = pending {
            finish_checkpoint(&mut state, &state_path, work, worker)?;
        }
        Ok(())
    })?;
    remove_any(&work.join("active.git"))?;
    remove_any(&work.join("active-project"))?;
    remove_any(&work.join("repos"))?;
    remove_any(&work.join("downloads"))?;
    if raw {
        let source = work.join("source");
        for p in &plan.projects { place_files(p, &source)?; }
        guard.check()?;
        eprintln!("COMPLETE: {}", source.display());
        return Ok(());
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
    let raw = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [flag] if flag == "--raw" => true,
        _ => { eprintln!("ERROR: expected --raw or no arguments"); std::process::exit(1); }
    };
    if let Err(err) = build(raw) { eprintln!("ERROR: {err}"); std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_project() {
        let work = std::env::temp_dir().join(format!("squashfs-empty-{}", std::process::id()));
        fs::create_dir(&work).unwrap();
        let guard = Guard { root: work.clone(), floor: 0 };
        let dir = work.join("downloads/000000");
        fs::create_dir_all(&dir).unwrap();
        let mut tar = Command::new("tar");
        tar.arg("-czf").arg(dir.join("archive.tar.gz")).args(["--files-from", "/dev/null"]);
        guard.run(tar).unwrap();
        let sha = "0".repeat(40);
        let project = Project { path: "empty".into(), url: "https://android.googlesource.com/empty".into(),
            revision: sha.clone(), copyfiles: vec![], linkfiles: vec![] };
        for raw in [false, true] {
            let stage = stage_project(&project, std::slice::from_ref(&project), &work,
                                      &guard, &sha, 0, b"{}", raw).unwrap();
            assert!(stage.join("empty").is_dir());
            assert_eq!(fs::read_to_string(stage.join(".source-image/commits/000000")).unwrap(),
                       format!("{sha}\n"));
        }
        fs::remove_dir_all(work).unwrap();
    }
    #[test]
    fn placements_are_repeatable() {
        let root = std::env::temp_dir().join(format!("lineage-placements-{}", std::process::id()));
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/file"), "source").unwrap();
        let project = Project { path: "a".into(), url: "unused".into(), revision: "unused".into(),
            copyfiles: vec![Placement { src: "file".into(), dest: "z/copy".into() }],
            linkfiles: vec![Placement { src: "file".into(), dest: "z/link".into() }] };
        for _ in 0..2 { place_files(&project, &root).unwrap(); }
        assert_eq!(fs::read_to_string(root.join("z/copy")).unwrap(), "source");
        assert_eq!(fs::metadata(root.join("z/copy")).unwrap().permissions().mode() & 0o222, 0);
        assert_eq!(fs::read_link(root.join("z/link")).unwrap(), Path::new("../a/file"));
        fs::remove_dir_all(root).unwrap();
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
