#!/usr/bin/env bash

set -uexo pipefail

mkdir out prebuilts external .repo

until sudo mount -t nfs "[$out]:/out"              out; do sleep 5; done
until sudo mount -t nfs "[$repo]:/.repo"         .repo; do sleep 5; done
until sudo mount -t nfs "[$repo]:/prebuilts" prebuilts; do sleep 5; done
until sudo mount -t nfs "[$repo]:/external"   external; do sleep 5; done

echo 'testing without building'

touch out/done
touch .repo/done
