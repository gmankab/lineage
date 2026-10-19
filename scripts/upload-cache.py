import os
import pathlib
import subprocess
import sys

src_str: str = sys.argv[-1]
src_path = pathlib.Path(src_str)
chunks = pathlib.Path(os.environ['RUNNER_TEMP']) / f'{src_str}-oci'
chunks.mkdir(parents=True, exist_ok=True)

with (
    subprocess.Popen(
        ['split', '-b', '1G', '-d', '-a', '3', '-', str(chunks / 'part-')],
        stdin=subprocess.PIPE,
    ) as split,
    subprocess.Popen(
        ['tar', '--null', '-T', '-', '--remove-files', '-cf', '-'],
        cwd=src_path, stdin=subprocess.PIPE, stdout=split.stdin,
    ) as tar,
):
    assert split.stdin
    assert tar.stdin
    split.stdin.close()
    for path in src_path.rglob('*'):
        if path.is_file() or path.is_symlink():
            tar.stdin.write(os.fsencode(path.relative_to(src_path)) + b'\0')
    tar.stdin.close()

for process in (tar, split):
    subprocess.CompletedProcess(process.args, process.returncode).check_returncode()
