'''
1. generates yggdrasil config
2. gets if from this configs
3. writes privatekey and ip to github output
'''

import subprocess
import shutil
import json
import os
from pathlib import Path


class common:
    github_output: Path = Path(
        os.getenv('GITHUB_OUTPUT', 'test_github_output.env')
    )
    __yggdrasil__ = shutil.which('yggdrasil')
    assert __yggdrasil__
    yggdrasil: Path = Path(__yggdrasil__)


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


def write_output(prefix: str):
    private_key = generate_private_key()
    ip = get_ip(private_key)
    with common.github_output.open('a') as output:
        output.write(f'{prefix}_privatekey={private_key}\n')
        output.write(f'{prefix}_ip={ip}\n')


def main():
    write_output('builder')
    write_output('repo')
    write_output('out')


main()
