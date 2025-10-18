'''
1. generates yggdrasil configs
2. gets if from this configs
3. writes privatekey to repo secrets and ip to repo variables
'''

import subprocess
import shutil
import json
import os
from pathlib import Path


class common:
    repo: str = os.getenv('GITHUB_REPOSITORY', '')
    assert repo
    _yggdrasil = shutil.which('yggdrasil')
    assert _yggdrasil
    yggdrasil: Path = Path(_yggdrasil)
    _gh = shutil.which('gh')
    assert _gh
    gh: Path = Path(_gh)


def get_ip(private_key: str) -> str:
    return subprocess.check_output(
        [common.yggdrasil, "-address", "-useconf"],
        input=json.dumps({'PrivateKey': private_key}),
        text=True,
    ).strip()


def generate_private_key() -> str:
    return json.loads(
        subprocess.check_output(
            [common.yggdrasil, '-genconf', '-json'],
            text=True,
        )
    )['PrivateKey'].strip()


def write_secrets(prefix: str):
    private_key = generate_private_key()
    ip = get_ip(private_key)
    subprocess.run(
        args=['gh', 'secret', 'set', '--repo', common.repo, f'{prefix}_privatekey'],
        input=private_key,
        check=True,
        text=True,
    )
    subprocess.run(
        args=['gh', 'variable', 'set', '--repo', common.repo, f'{prefix}_ip'],
        input=ip,
        check=True,
        text=True,
    )


def main():
    write_secrets('builder')
    write_secrets('repo')
    write_secrets('out')


main()
