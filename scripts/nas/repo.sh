#!/usr/bin/env bash

set -uexo pipefail

sudo mkdir -p /etc/exports.d
sudo mkdir -p $(pwd)/export/.repo
sudo mkdir -p $(pwd)/export/prebuilts
sudo mkdir -p $(pwd)/export/external
sudo chown 1000 $(pwd)/export/*
echo "$GITHUB_WORKSPACE/export $builder(rw,async,fsid=0)" | sudo tee /etc/exports.d/nfs.exports
sudo systemctl enable --now nfs-server
sudo exportfs -rav

export file=$(pwd)/export/.repo/done
bash scripts/nas/wait.sh
