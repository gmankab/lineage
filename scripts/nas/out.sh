#!/usr/bin/env bash

set -uexo pipefail

sudo mkdir -p /etc/exports.d
sudo mkdir -p $GITHUB_WORKSPACE/export/out
sudo chown 1000 $GITHUB_WORKSPACE/export/*
echo "$GITHUB_WORKSPACE/export $builder(rw,async,fsid=0)" | sudo tee /etc/exports.d/nfs.exports
sudo systemctl enable --now nfs-server
sudo exportfs -rav
