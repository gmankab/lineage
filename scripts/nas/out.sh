#!/usr/bin/env bash

set -uexo pipefail

sudo mkdir -p /etc/exports.d
sudo mkdir -p $(pwd)/export/out
sudo chown 1000 $(pwd)/export/*
echo "$(pwd)/export $builder(rw,async,fsid=0)" | sudo tee /etc/exports.d/nfs.exports
sudo systemctl enable --now nfs-server
sudo exportfs -rav

export file=$(pwd)/export/out/done
bash scripts/nas/wait.sh
