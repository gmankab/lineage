#!/usr/bin/env bash

set -uexo pipefail

bash scripts/builder/mount.sh
bash scripts/builder/prepare.sh
bash scripts/builder/build.sh

touch out/done
touch .repo/done
