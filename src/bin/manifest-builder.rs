use serde::Serialize;
use std::{collections::{BTreeMap, HashMap, HashSet}, error::Error, fs, path::{Path, PathBuf}, process::Command};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MANIFEST: &str = "https://codeload.github.com/LineageOS/android/tar.gz/refs/heads/lineage-24.0";

struct Node {
    name: String,
    attrs: BTreeMap<String, String>,
    children: Vec<Node>,
}
impl Node {
    fn attr(&self, key: &str) -> Option<&str> { self.attrs.get(key).map(String::as_str) }
    fn required(&self, key: &str) -> Result<&str> {
        self.attr(key).ok_or_else(|| format!("<{}> missing {key}", self.name).into())
    }
}

fn unescape(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at + 1..].find(';').ok_or("unterminated XML entity")? + at + 1;
        let ent = &rest[at + 1..end];
        match ent {
            "amp" => out.push('&'), "lt" => out.push('<'), "gt" => out.push('>'),
            "quot" => out.push('"'), "apos" => out.push('\''),
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x") { u32::from_str_radix(h, 16)? }
                        else { ent.strip_prefix('#').ok_or("unknown XML entity")?.parse()? };
                out.push(char::from_u32(n).ok_or("invalid XML character")?);
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn start_tag(raw: &str) -> Result<Node> {
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
    let name = raw[..i].to_owned();
    if name.is_empty() { return Err("empty XML element".into()); }
    let mut attrs = BTreeMap::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i == b.len() { break; }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' { i += 1; }
        if start == i { return Err("invalid XML attribute".into()); }
        let key = &raw[start..i];
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if b.get(i) != Some(&b'=') { return Err("expected XML '='".into()); }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        let quote = *b.get(i).ok_or("expected XML quote")?;
        if quote != b'\'' && quote != b'"' { return Err("expected quoted XML attribute".into()); }
        i += 1;
        let begin = i;
        while i < b.len() && b[i] != quote { i += 1; }
        if i == b.len() { return Err("unterminated XML attribute".into()); }
        if attrs.insert(key.to_owned(), unescape(&raw[begin..i])?).is_some() {
            return Err(format!("duplicate XML attribute: {key}").into());
        }
        i += 1;
    }
    Ok(Node { name, attrs, children: Vec::new() })
}

fn parse_xml(xml: &str) -> Result<Node> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut pos = 0;
    while let Some(relative) = xml[pos..].find('<') {
        pos += relative;
        if xml[pos..].starts_with("<!--") {
            pos += xml[pos..].find("-->").ok_or("unterminated XML comment")? + 3;
            continue;
        }
        if xml[pos..].starts_with("<?") {
            pos += xml[pos..].find("?>").ok_or("unterminated XML header")? + 2;
            continue;
        }
        if xml[pos..].starts_with("<!") { return Err("XML DTD/entities unsupported".into()); }
        let bytes = xml.as_bytes();
        let mut end = pos + 1;
        let mut quote = 0;
        while end < bytes.len() {
            let b = bytes[end];
            if quote == 0 && (b == b'\'' || b == b'"') { quote = b; }
            else if b == quote { quote = 0; }
            else if quote == 0 && b == b'>' { break; }
            end += 1;
        }
        if end == bytes.len() { return Err("unclosed XML element".into()); }
        let raw = xml[pos + 1..end].trim();
        pos = end + 1;
        if let Some(name) = raw.strip_prefix('/') {
            let node = stack.pop().ok_or("unexpected XML close tag")?;
            if node.name != name.trim() { return Err(format!("mismatched XML close tag: {name}").into()); }
            if let Some(parent) = stack.last_mut() { parent.children.push(node); }
            else if root.replace(node).is_some() { return Err("multiple XML roots".into()); }
        } else {
            let empty = raw.ends_with('/');
            let content = if empty { raw[..raw.len() - 1].trim_end() } else { raw };
            let node = start_tag(content)?;
            if empty {
                if let Some(parent) = stack.last_mut() { parent.children.push(node); }
                else if root.replace(node).is_some() { return Err("multiple XML roots".into()); }
            } else { stack.push(node); }
        }
    }
    if !stack.is_empty() { return Err("unclosed XML tags".into()); }
    root.ok_or_else(|| "empty XML file".into())
}

fn included(dir: &Path, filename: &str, visited: &mut HashSet<PathBuf>, out: &mut Vec<Node>) -> Result<()> {
    if filename.starts_with('/') || filename.split('/').any(|s| s.is_empty() || s == ".." || s == ".") {
        return Err(format!("unsafe XML include: {filename}").into());
    }
    let file = dir.join(filename).canonicalize()?;
    if !file.starts_with(dir) || !visited.insert(file.clone()) { return Err("invalid/repeated XML include".into()); }
    let root = parse_xml(&fs::read_to_string(file)?)?;
    if root.name != "manifest" { return Err("expected <manifest> root".into()); }
    for node in root.children {
        if node.name == "include" {
            included(dir, node.required("name")?, visited, out)?;
        } else if ["project", "remote", "default", "contactinfo"].contains(&node.name.as_str()) {
            out.push(node);
        } else { return Err(format!("unsupported Repo XML element: <{}>", node.name).into()); }
    }
    Ok(())
}

#[derive(Serialize)]
struct Placement { src: String, dest: String }
#[derive(Serialize)]
struct Project {
    path: String, url: String, revision: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    copyfiles: Vec<Placement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    linkfiles: Vec<Placement>,
}
#[derive(Serialize)]
struct Plan { version: u8, projects: Vec<Project> }

fn project(node: &Node, remotes: &HashMap<&str, &Node>, default: &Node) -> Result<Option<Project>> {
    let groups: Vec<_> = node.attr("groups").unwrap_or("").split(',').map(str::trim).collect();
    if groups.contains(&"notdefault") && !groups.contains(&"platform-linux") {
        return Ok(None);
    }
    let name = node.required("name")?;
    let path = node.attr("path").unwrap_or(name).to_owned();
    let remote_name = node.attr("remote").or_else(|| default.attr("remote"))
        .ok_or("missing remote")?;
    let remote = remotes.get(remote_name).ok_or("unknown remote")?;
    let fetch = remote.required("fetch")?;
    let url = match fetch {
        ".." => format!("https://github.com/{name}"),
        f if f.starts_with("https://") =>
            format!("{}/{name}", f.trim_end_matches('/')),
        _ => return Err(format!("unsupported remote fetch URL: {fetch}").into()),
    };
    let mut revision = node.attr("revision").or_else(|| remote.attr("revision"))
        .or_else(|| default.attr("revision")).ok_or("missing revision")?.to_owned();
    // Preserve the Android 17 libcxxabi pin in plan.json when regenerating.
    if url == "https://android.googlesource.com/platform/external/libcxxabi"
        && revision == "refs/tags/android-17.0.0_r1" {
        revision = "84450eba5a24fe5b7df77ed1307bfada05b14dc7".into();
    }
    if node.attr("sync-s") == Some("true") { return Err(format!("submodules unsupported: {path}").into()); }
    let mut p = Project { path, url, revision, copyfiles: Vec::new(), linkfiles: Vec::new() };
    for child in &node.children {
        let item = Placement { src: child.required("src")?.to_owned(), dest: child.required("dest")?.to_owned() };
        match child.name.as_str() {
            "copyfile" => p.copyfiles.push(item), "linkfile" => p.linkfiles.push(item),
            other => return Err(format!("unsupported project element: <{other}>").into()),
        }
    }
    Ok(Some(p))
}

fn command(name: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(name).args(args).status()?;
    if !status.success() { return Err(format!("{name}: {status}").into()); }
    Ok(())
}

fn run() -> Result<()> {
    if std::env::args_os().len() != 1 { return Err("manifest-builder takes no arguments".into()); }
    if Path::new("plan.json").exists() { return Err("plan.json exists; remove it to regenerate".into()); }
    let dir = Path::new("manifests");
    if !dir.exists() {
        let tmp = Path::new("manifests.tmp");
        if tmp.exists() { fs::remove_dir_all(tmp)?; }
        fs::create_dir(tmp)?;
        fs::create_dir(tmp.join("source"))?;
        command("curl", &["-fLsS", "--retry", "3", "--proto", "=https", "--proto-redir", "=https",
            "-o", "manifests.tmp/archive.tar.gz", MANIFEST])?;
        command("tar", &["-xzf", "manifests.tmp/archive.tar.gz", "-C", "manifests.tmp/source",
            "--strip-components=1", "--no-same-owner"])?;
        fs::rename(tmp.join("source"), dir)?;
        fs::remove_dir_all(tmp)?;
    }
    let dir = dir.canonicalize()?;
    let mut nodes = Vec::new();
    included(&dir, "default.xml", &mut HashSet::new(), &mut nodes)?;
    let mut remotes = HashMap::new();
    let mut default = None;
    for n in &nodes {
        if n.name == "remote" {
            remotes.insert(n.required("name")?, n);
        } else if n.name == "default" {
            if default.replace(n).is_some() { return Err("multiple <default> entries".into()); }
        }
    }
    let default = default.ok_or("missing default")?;
    let mut projects: Vec<_> = nodes.iter().filter(|n| n.name == "project")
        .filter_map(|n| project(n, &remotes, default).transpose()).collect::<Result<_>>()?;
    projects.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    if projects.is_empty() || projects.windows(2).any(|w| w[0].path == w[1].path) {
        return Err("empty/duplicate manifest project paths".into());
    }
    let plan = Plan { version: 1, projects };
    fs::write("plan.json", format!("{}\n", serde_json::to_string_pretty(&plan)?))?;
    eprintln!("plan.json: {} projects", plan.projects.len());
    Ok(())
}

fn main() {
    if let Err(err) = run() { eprintln!("ERROR: {err}"); std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_xml_entities() {
        let root = parse_xml("<?xml version='1.0'?><manifest><!-- c --><project name='a&amp;b'><linkfile src='a' dest='b'/></project></manifest>").unwrap();
        assert_eq!(root.children[0].required("name").unwrap(), "a&b");
        assert_eq!(root.children[0].children.len(), 1);
    }
    #[test]
    fn rejects_bad_xml() {
        assert!(parse_xml("<manifest><project></manifest>").is_err());
        assert!(parse_xml("<!DOCTYPE foo><manifest/>").is_err());
    }
}
