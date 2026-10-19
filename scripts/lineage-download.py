#!/usr/bin/env python3

import concurrent.futures
import json
import os
import re
import shutil
import subprocess
import tempfile
import threading
import time
import xml.etree.ElementTree
from collections.abc import Iterator
from pathlib import Path
from urllib.parse import quote


# ruff: disable[RUF012]
class config:
    token = os.environ['GH_TOKEN']
    workdir = Path('workdir')
    manifest: Path = workdir / 'manifests/default.xml'
    source: Path = workdir / 'source'
    jobs: int = 16
    gitiles_lock: threading.Lock = threading.Lock()
    gitiles_next: float = 0.0
    webviews: set[str] = {
        f'LineageOS/android_external_chromium-webview_prebuilt_{arch}'
        for arch in ('arm', 'arm64', 'x86', 'x86_64')
    }


class Projects:
    def __init__(
        self,
    ) -> None:
        self.remotes: dict[str, xml.etree.ElementTree.Element] = {}
        self.result: list[xml.etree.ElementTree.Element] = []
        self.default: xml.etree.ElementTree.Element

    def project_path(
        self,
        node: xml.etree.ElementTree.Element,
    ) -> str:
        return node.attrib['path']

    def parse_file(
        self,
        file: Path,
    ) -> list[xml.etree.ElementTree.Element]:
        for node in included(file, file.parent):
            match node.tag:
                case 'remote':
                    self.remotes[node.attrib['name']] = node
                case 'default':
                    self.default = node
                case 'project':
                    self.parse_node(node)
        return sorted(
            self.result,
            key=self.project_path,
        )

    def parse_node(
        self,
        node: xml.etree.ElementTree.Element
    ) -> None:
        groups = node.get('groups', '').replace(',', ' ').split()
        if 'notdefault' in groups and 'platform-linux' not in groups:
            return
        remote = self.remotes[node.get('remote', self.default.attrib['remote'])]
        fetch = remote.attrib['fetch']
        url = f"{'https://github.com' if fetch == '..' else fetch.rstrip('/')}/{node.attrib['name']}"
        revision = node.get('revision') or remote.get('revision') or self.default.attrib['revision']
        node.set('path', node.get('path', node.attrib['name']))
        node.set('url', url)
        node.set('revision', revision)
        self.result.append(node)


def curl(
    url: str,
    *args: str,
    output: str|Path = '',
) -> bytes:
    cmd = [
        'curl', '-q', '-fLsS', '--retry', '3', '--connect-timeout', '30',
        '--speed-limit', '1024', '--speed-time', '120', '--globoff',
        '--proto', '=https', '--proto-redir', '=https', '--url', url,
    ]
    if url.startswith('https://api.github.com/'):
        cmd += ['--header', f'Authorization: Bearer {config.token}']
    if not output:
        cmd += [
            '--max-time', '120', '--retry', '8', '--retry-all-errors',
            '--retry-max-time', '180',
        ]
    else:
        cmd += ['--output', str(output)]
    return subprocess.run(
        [*cmd, *args],
        check=True,
        stdout=subprocess.PIPE
    ).stdout


def unpack(
    archive: Path,
    dest: Path,
    github: bool = True,
) -> None:
    dest.mkdir()
    args: list[str] = [
        'tar', '-xzf', str(archive), '-C', str(dest), '--no-same-owner',
    ]
    if github:
        args.append('--strip-components=1')
    subprocess.run(args, check=True)
    archive.unlink()


def included(
    file: str|Path,
    root: Path,
) -> Iterator[xml.etree.ElementTree.Element]:
    for node in xml.etree.ElementTree.parse(file).getroot():
        if node.tag == 'include':
            yield from included(root / node.attrib['name'], root)
        else:
            yield node


def commit(
    url: str,
    revision: str,
) -> str:
    if re.fullmatch(r'[0-9a-fA-F]{40}|[0-9a-fA-F]{64}', revision):
        return revision.lower()
    if url.startswith('https://github.com/'):
        repo: str = url.removeprefix('https://github.com/')
        return curl(
            f'https://api.github.com/repos/{repo}/commits/{quote(revision, safe="")}',
            '--header',
            'Accept: application/vnd.github.sha',
        ).decode().strip()
    with config.gitiles_lock:
        time.sleep(max(0, config.gitiles_next - time.monotonic()))
        config.gitiles_next = time.monotonic() + 0.5
    target: str = '+refs/' + revision[5:] if revision.startswith('refs/') else '+show/' + revision
    url: str = f'{url}/{quote(target, safe="/+")}?format=JSON'
    data = json.loads(
        curl(url).split(b'\n', 1)[1]
    )
    if revision.startswith('refs/'):
        data = data[revision]
    return (
        data.get('peeled')
        or data.get('value')
        or data.get('commit')
    ).lower()


def lfs(
    repo: str,
    dest: Path,
) -> None:
    file: Path = dest / 'webview.apk'
    pointer: list[str] = file.read_text().splitlines()
    oid: str = pointer[1].removeprefix('oid sha256:')
    size: int = int(pointer[2].removeprefix('size '))
    endpoint: str = f'https://review.lineageos.org/{repo}.git/info/lfs'
    payload: dict = {
        'operation': 'download',
        'objects': [
            {
                'oid': oid,
                'size': size
            }
        ]
    }
    response: bytes = curl(
        endpoint + '/objects/batch',
        '--header', 'Accept: application/vnd.git-lfs+json',
        '--header', 'Content-Type: application/vnd.git-lfs+json',
        '--data-binary',
        json.dumps(payload),
    )
    batch = json.loads(response)
    action = batch['objects'][0]['actions']['download']
    headers = [
        arg for key, value in action.get('header', {}).items()
        for arg in ('--header', f'{key}: {value}')
    ]
    curl(
        action['href'],
        *headers,
        output=file,
    )


def download(
    node: xml.etree.ElementTree.Element,
) -> str:
    name: str = node.attrib['path']
    url: str = node.attrib['url']
    revision: str = node.attrib['revision']
    dest: Path = config.source / name
    if dest.exists():
        return name
    sha: str = commit(url, revision)
    with tempfile.TemporaryDirectory(prefix='.tmp-', dir=config.workdir) as tmp:
        tmp = Path(tmp)
        if url.startswith('https://github.com/'):
            repo: str = url.removeprefix('https://github.com/')
            archive: str = f'https://codeload.github.com/{repo}/tar.gz/{sha}'
        else:
            repo: str = ''
            archive: str = f'{url}/+archive/{sha}.tar.gz'
        curl(
            url=archive,
            output=tmp/'archive.tar.gz',
        )
        unpack(
            archive=tmp/'archive.tar.gz',
            dest=tmp/'source',
            github=bool(repo),
        )
        if repo and repo in config.webviews:
            lfs(repo, tmp / 'source')
        dest.parent.mkdir(
            parents=True,
            exist_ok=True,
        )
        source = tmp / 'source'
        source.rename(dest)
    return name


def place_files(
    nodes: list[xml.etree.ElementTree.Element],
) -> None:
    for tag in ('copyfile', 'linkfile'):
        for node in nodes:
            root: Path = config.source / node.attrib['path']
            for placement in node.findall(tag):
                if placement.attrib['src'] == '.':
                    src: Path = root
                else:
                    src: Path = root / placement.attrib['src']
                dest: Path = config.source / placement.attrib['dest']
                dest.parent.mkdir(
                    parents=True,
                    exist_ok=True,
                )
                dest.unlink(
                    missing_ok=True,
                )
                if tag == 'copyfile':
                    shutil.copy2(
                        src=src,
                        dst=dest,
                    )
                    dest.chmod(
                        dest.stat().st_mode & ~0o222
                    )
                else:
                    dest.symlink_to(
                        os.path.relpath(
                            path=src,
                            start=dest.parent
                        )
                    )


def main() -> None:
    config.workdir.mkdir(
        exist_ok=True,
    )
    if not config.manifest.exists():
        archive = config.workdir / 'manifest.tar.gz'
        curl(
            url='https://codeload.github.com/LineageOS/android/tar.gz/refs/heads/lineage-24.0',
            output=archive,
        )
        unpack(
            archive=archive,
            dest=config.workdir/'manifests',
        )
    projects = Projects()
    nodes: list[xml.etree.ElementTree.Element] = projects.parse_file(config.manifest)
    total: int = len(nodes)
    config.source.mkdir(
        exist_ok=True,
    )
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=config.jobs,
    ) as pool:
        futures = [
            pool.submit(download, node)
            for node in nodes
        ]
        for index, future in enumerate(
            concurrent.futures.as_completed(futures),
            start=1,
        ):
            print(
                f'[{index}/{total}] {future.result()}',
                flush=True,
            )
    place_files(nodes)
    print('done')


if __name__ == '__main__':
    main()
