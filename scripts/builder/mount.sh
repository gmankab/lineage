#!/usr/bin/env bash

set -uexo pipefail

mkdir out prebuilts external .repo
sudo mount -t nfs "[$out]:/external" out
sudo mount -t nfs "[$repo]:/external" prebuilts
sudo mount -t nfs "[$repo]:/external" external
sudo mount -t nfs "[$repo]:/external" .repo
