use crate::{check_parents, oid, remove_any, safe_path, save_json, Guard, Project, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::{
    fs::{self, File, Permissions},
    io::Read,
    os::unix::{ffi::OsStrExt, fs::{symlink, PermissionsExt}},
    path::Path,
    process::Command,
};

#[derive(Deserialize, Serialize)]
struct Entry {
    path: String,
    mode: String,
    sha: String,
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    truncated: bool,
}

fn encode(s: &str, slash: bool) -> String {
    s.bytes().map(|b| {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || slash && b == b'/' {
            (b as char).to_string()
        } else { format!("%{b:02X}") }
    }).collect()
}

fn github(p: &Project) -> Option<&str> {
    p.url.strip_prefix("https://github.com/")
        .map(|repo| repo.trim_end_matches('/').trim_end_matches(".git"))
}

fn webview(repo: &str) -> bool {
    matches!(repo.strip_prefix("LineageOS/android_external_chromium-webview_prebuilt_"),
        Some("arm" | "arm64" | "x86" | "x86_64"))
}

fn curl(url: &str) -> Command {
    let mut cmd = Command::new("curl");
    cmd.args(["-q", "-fLsS", "--retry", "3", "--connect-timeout", "30",
        "--speed-limit", "1024", "--speed-time", "120", "--proto", "=https",
        "--proto-redir", "=https", "--globoff", "--url", url]);
    if url.starts_with("https://api.github.com/") || url.starts_with("https://github.com/") {
        if let Some(token) = std::env::var("GH_TOKEN").ok().filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()))
        {
            cmd.arg("--header").arg(format!("Authorization: Bearer {token}"));
        }
    }
    cmd
}

fn request(mut cmd: Command, guard: &Guard) -> Result<Vec<u8>> {
    guard.check()?;
    let out = cmd.args(["--max-time", "120"]).output()?;
    guard.check()?;
    if !out.status.success() { return Err(format!("HTTP metadata request failed ({})", out.status).into()); }
    Ok(out.stdout)
}

fn download(mut cmd: Command, path: &Path, guard: &Guard) -> Result<()> {
    let tmp = path.with_extension("part");
    remove_any(&tmp)?;
    cmd.arg("--output").arg(&tmp);
    guard.run(cmd)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().ok_or("download has no parent")?)?.sync_all()?;
    Ok(())
}

fn tree(repo: &str, sha: &str, prefix: &str, out: &mut Vec<Entry>, guard: &Guard) -> Result<()> {
    let url = format!("https://api.github.com/repos/{repo}/git/trees/{sha}");
    let mut data: Tree = serde_json::from_slice(&request(curl(&format!("{url}?recursive=1")), guard)?)?;
    if data.truncated {
        data = serde_json::from_slice(&request(curl(&url), guard)?)?;
        if data.truncated { return Err("non-recursive GitHub tree is truncated".into()); }
        for mut entry in data.tree {
            entry.path.insert_str(0, prefix);
            if entry.mode == "040000" {
                tree(repo, &entry.sha, &format!("{}/", entry.path), out, guard)?;
            }
            out.push(entry);
        }
    } else {
        for mut entry in data.tree {
            entry.path.insert_str(0, prefix);
            out.push(entry);
        }
    }
    Ok(())
}

fn hash_file<D: Digest>(path: &Path, prefix: &[u8]) -> Result<String> {
    let mut hash = D::new();
    hash.update(prefix);
    let mut file = File::open(path)?;
    let mut buf = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 { break; }
        hash.update(&buf[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn blob_hash(path: &Path, mode: &str) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if mode == "120000" && meta.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let bytes = target.as_os_str().as_bytes();
        let mut hash = Sha1::new();
        hash.update(format!("blob {}\0", bytes.len()));
        hash.update(bytes);
        return Ok(format!("{:x}", hash.finalize()));
    }
    if !matches!(mode, "100644" | "100755") || !meta.is_file() {
        return Err("unexpected source file type".into());
    }
    hash_file::<Sha1>(path, format!("blob {}\0", meta.len()).as_bytes())
}

fn pointer(bytes: &[u8]) -> Result<(String, u64)> {
    let text = std::str::from_utf8(bytes)?;
    let mut lines = text.lines();
    if lines.next() != Some("version https://git-lfs.github.com/spec/v1") {
        return Err("WebView APK is not the expected LFS pointer".into());
    }
    let hash = lines.next().and_then(|s| s.strip_prefix("oid sha256:"))
        .ok_or("missing WebView LFS object ID")?;
    let size: u64 = lines.next().and_then(|s| s.strip_prefix("size "))
        .ok_or("missing WebView LFS object size")?.parse()?;
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || lines.next().is_some() {
        return Err("unsupported WebView LFS pointer".into());
    }
    Ok((hash.to_ascii_lowercase(), size))
}

fn lfs(repo: &str, sha: &str, dir: &Path, guard: &Guard) -> Result<()> {
    // These four APKs are the current plan's LFS files; never hydrate pointer test fixtures.
    let raw = format!("https://raw.githubusercontent.com/{repo}/{sha}");
    let endpoint = format!("https://review.lineageos.org/{repo}.git/info/lfs");
    let config = String::from_utf8(request(curl(&format!("{raw}/.lfsconfig")), guard)?)?;
    if !config.lines().any(|line| line.trim() == format!("url = \"{endpoint}\"")) {
        return Err("WebView .lfsconfig no longer matches the supported LineageOS LFS server".into());
    }
    let (hash, size) = pointer(&request(curl(&format!("{raw}/webview.apk")), guard)?)?;
    let path = dir.join("webview.apk");
    if fs::metadata(&path).is_ok_and(|m| m.len() == size)
        && hash_file::<Sha256>(&path, b"").ok().as_deref() == Some(&hash)
    {
        return Ok(());
    }
    remove_any(&path)?;
    let mut cmd = curl(&format!("{endpoint}/objects/batch"));
    cmd.args(["--header", "Accept: application/vnd.git-lfs+json",
        "--header", "Content-Type: application/vnd.git-lfs+json", "--data-binary"])
        .arg(json!({"operation":"download", "objects":[{"oid":hash,"size":size}]}).to_string());
    let batch: Value = serde_json::from_slice(&request(cmd, guard)?)?;
    let object = &batch["objects"][0];
    if object["oid"].as_str() != Some(&hash) || object["size"].as_u64() != Some(size)
        || !object["error"].is_null()
    {
        return Err("LineageOS LFS server did not provide the requested WebView object".into());
    }
    let action = &object["actions"]["download"];
    let mut cmd = curl(action["href"].as_str().ok_or("missing WebView LFS download URL")?);
    if let Some(headers) = action["header"].as_object() {
        for (name, value) in headers {
            let value = value.as_str().ok_or("invalid LFS download header")?;
            if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
                return Err("invalid LFS download header".into());
            }
            cmd.arg("--header").arg(format!("{name}: {value}"));
        }
    }
    download(cmd, &path, guard)?;
    if fs::metadata(&path)?.len() != size || hash_file::<Sha256>(&path, b"")? != hash {
        remove_any(&path)?;
        return Err("WebView LFS download failed its size/SHA-256 check".into());
    }
    Ok(())
}

pub fn fetch(p: &Project, dir: &Path, guard: &Guard) -> Result<String> {
    fs::create_dir_all(dir)?;
    let repo = github(p);
    if repo.is_none() && !p.url.starts_with("https://android.googlesource.com/") {
        return Err(format!("unsupported archive host: {}", p.url).into());
    }
    let pin = dir.join("commit.json");
    let sha: String = if pin.is_file() { serde_json::from_reader(File::open(&pin)?)? }
    else if oid(&p.revision) { p.revision.to_ascii_lowercase() }
    else {
        let revision = encode(&p.revision, false);
        if let Some(repo) = repo {
            let mut cmd = curl(&format!("https://api.github.com/repos/{repo}/commits/{revision}"));
            cmd.args(["--header", "Accept: application/vnd.github.sha"]);
            String::from_utf8(request(cmd, guard)?)?.trim().to_string()
        } else {
            let url = format!("{}/+/{revision}?format=JSON", p.url.trim_end_matches('/'));
            let bytes = request(curl(&url), guard)?;
            let bytes = bytes.splitn(2, |b| *b == b'\n').nth(1).ok_or("invalid Gitiles JSON")?;
            let value: Value = serde_json::from_slice(bytes)?;
            value["commit"].as_str().ok_or("revision does not resolve to a commit")?.to_string()
        }
    };
    if !oid(&sha) || oid(&p.revision) && !sha.eq_ignore_ascii_case(&p.revision) {
        return Err(format!("revision mismatch for {}", p.path).into());
    }
    if !pin.is_file() { save_json(&pin, &sha)?; }
    if let Some(repo) = repo {
        let path = dir.join("tree.json");
        if !path.is_file() {
            let mut entries = Vec::new();
            tree(repo, &sha, "", &mut entries, guard)?;
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            for entry in &entries { safe_path(&entry.path)?; }
            save_json(&path, &entries)?;
        }
        if webview(repo) { lfs(repo, &sha, dir, guard)?; }
    }
    let archive = dir.join("archive.tar.gz");
    if !archive.is_file() {
        let url = if let Some(repo) = repo {
            format!("https://codeload.github.com/{repo}/tar.gz/{sha}")
        } else { format!("{}/+archive/{sha}.tar.gz", p.url.trim_end_matches('/')) };
        download(curl(&url), &archive, guard)?;
    }
    Ok(sha)
}

pub fn unpack(p: &Project, dir: &Path, dest: &Path, sha: &str, guard: &Guard) -> Result<()> {
    let repo = github(p);
    let mut cmd = Command::new("tar");
    cmd.args(["--extract", "--gzip", "--no-same-owner", "--file"])
        .arg(dir.join("archive.tar.gz")).arg("--directory").arg(dest);
    if repo.is_some() { cmd.arg("--strip-components=1"); }
    if let Err(err) = guard.run(cmd) {
        remove_any(&dir.join("archive.tar.gz"))?;
        return Err(err);
    }
    if let Some(repo) = repo {
        let entries: Vec<Entry> = serde_json::from_reader(File::open(dir.join("tree.json"))?)?;
        for entry in entries {
            safe_path(&entry.path)?;
            let path = check_parents(dest, &entry.path)?;
            match entry.mode.as_str() {
                "040000" | "160000" => {
                    if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) { remove_any(&path)?; }
                    fs::create_dir_all(&path)?;
                }
                "100644" | "100755" | "120000" => {
                    if blob_hash(&path, &entry.mode).ok().as_deref() != Some(&entry.sha) {
                        let blob = dir.join("restore");
                        let url = format!("https://raw.githubusercontent.com/{repo}/{sha}/{}", encode(&entry.path, true));
                        download(curl(&url), &blob, guard)?;
                        if blob_hash(&blob, "100644")? != entry.sha {
                            return Err(format!("source blob checksum mismatch: {}", entry.path).into());
                        }
                        remove_any(&path)?;
                        if entry.mode == "120000" {
                            use std::os::unix::ffi::OsStringExt;
                            symlink(std::ffi::OsString::from_vec(fs::read(&blob)?), &path)?;
                        } else { fs::copy(&blob, &path)?; }
                        fs::remove_file(blob)?;
                    }
                    if entry.mode != "120000" {
                        fs::set_permissions(&path, Permissions::from_mode(if entry.mode == "100755" { 0o755 } else { 0o644 }))?;
                    }
                    if entry.mode != "120000" && !webview(repo)
                        && path.file_name().is_some_and(|name| name == ".gitattributes")
                        && fs::read_to_string(&path)?.lines().any(|line| {
                            let line = line.trim();
                            !line.starts_with('#') && line.split_whitespace().any(|word| word == "filter=lfs")
                        })
                    {
                        return Err(format!("unsupported LFS tracking in {}", entry.path).into());
                    }
                }
                _ => return Err(format!("unsupported GitHub tree mode: {}", entry.mode).into()),
            }
            guard.check()?;
        }
        if webview(repo) {
            let path = check_parents(dest, "webview.apk")?;
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() { return Err("WebView APK is not a regular file".into()); }
            let (hash, size) = pointer(&fs::read(&path)?)?;
            let apk = dir.join("webview.apk");
            if fs::metadata(&apk)?.len() != size || hash_file::<Sha256>(&apk, b"")? != hash {
                return Err("cached WebView APK failed its size/SHA-256 check".into());
            }
            remove_any(&path)?;
            fs::copy(apk, &path)?;
            fs::set_permissions(path, meta.permissions())?;
        }
    }
    guard.check()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_requires_the_full_hash_and_size() {
        let good = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 123\n", "a".repeat(64));
        assert_eq!(pointer(good.as_bytes()).unwrap(), ("a".repeat(64), 123));
        assert!(pointer(good.replace("size 123", "size -1").as_bytes()).is_err());
        assert!(pointer(good.replacen("aaaa", "z", 1).as_bytes()).is_err());
        assert!(pointer(b"normal source file").is_err());
    }

    #[test]
    fn url_components_do_not_change_request_paths() {
        assert_eq!(encode("refs/heads/main", false), "refs%2Fheads%2Fmain");
        assert_eq!(encode("a b/#x?.rs", true), "a%20b/%23x%3F.rs");
        assert!(webview("LineageOS/android_external_chromium-webview_prebuilt_arm64"));
        assert!(!webview("LineageOS/android_external_chromium-webview_prebuilt_other"));
    }
}
