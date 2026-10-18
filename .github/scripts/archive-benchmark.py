#!/usr/bin/env python3
"""Compare shallow Git checkout against HTTP archive download + extraction."""
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import time

# Eight GitHub and eight Gitiles projects from plan.json, excluding large prebuilts.
SAMPLES = (
    "android", "art", "bionic", "bootable/recovery", "build/make",
    "external/libpng", "packages/apps/Settings", "system/core",
    "build/blueprint", "external/boringssl", "external/curl", "external/expat",
    "external/googletest", "external/protobuf", "external/sqlite", "external/zlib",
)


def run(*args):
    result = subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"exit {result.returncode}")


def archive_url(project):
    url, rev = project["url"].rstrip("/"), project["revision"]
    if url.startswith("https://github.com/"):
        return f"{url}/archive/{rev}.tar.gz", 1
    if url.startswith("https://android.googlesource.com/"):
        return f"{url}/+archive/{rev}.tar.gz", 0
    raise ValueError(f"unsupported archive host: {url}")


def benchmark(project, method):
    path = project["path"]
    tree = Path("bench/trees") / path
    tree.mkdir(parents=True, exist_ok=True)
    start = time.monotonic()
    try:
        if method == "git":
            repo = Path("bench/repos") / f"{path}.git"
            repo.parent.mkdir(parents=True, exist_ok=True)
            run("git", "init", "--bare", "-q", str(repo))
            cmd = ["git", f"--git-dir={repo}"]
            run(*cmd, "fetch", "-q", "--depth=1", "--no-tags", "--no-recurse-submodules",
                project["url"], project["revision"])
            downloaded = time.monotonic()
            size = sum(p.stat().st_size for p in repo.glob("objects/pack/*.pack"))
            run(*cmd, f"--work-tree={tree}", "-c", "core.bare=false", "checkout", "-q", "-f", "FETCH_HEAD")
        else:
            url, strip = archive_url(project)
            archive = Path("bench/archives") / f"{path}.tar.gz"
            archive.parent.mkdir(parents=True, exist_ok=True)
            run("curl", "-fLsS", "--retry", "3", "-o", str(archive), url)
            downloaded = time.monotonic()
            size = archive.stat().st_size
            args = ["tar", "-xzf", str(archive), "-C", str(tree)]
            if strip:
                args.append("--strip-components=1")
            run(*args)
            archive.unlink()
        return path, downloaded - start, time.monotonic() - downloaded, size, ""
    except Exception as exc:
        return path, 0, 0, 0, str(exc).replace("\n", " ")[:300]


def main():
    method = sys.argv[1]
    if method not in ("git", "archive"):
        sys.exit("expected git or archive")
    plan = {p["path"]: p for p in json.loads(Path("plan.json").read_text())["projects"]}
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        results = list(pool.map(lambda path: benchmark(plan[path], method), SAMPLES))
    elapsed = time.monotonic() - start
    lines = [f"### {method}: {elapsed:.1f}s total (16 concurrent projects)", "",
             "| Project | Download | Checkout/extract | Compressed bytes | Result |",
             "|---|---:|---:|---:|---|"]
    for path, download, checkout, size, error in results:
        lines.append(f"| {path} | {download:.1f}s | {checkout:.1f}s | {size:,} | {error or 'OK'} |")
    lines += ["", f"Successful: {sum(not r[4] for r in results)}/{len(results)}", ""]
    report = "\n".join(lines)
    print(report, flush=True)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(report)
    if any(r[4] for r in results):
        sys.exit(1)


if __name__ == "__main__":
    main()
