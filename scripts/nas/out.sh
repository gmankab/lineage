#!/usr/bin/env bash

set -uexo pipefail

sudo mkdir -p /etc/exports.d
sudo mkdir -p /var/export/out
sudo chown 1000 /var/export/*
echo "/var/export $builder(rw,async,fsid=0)" | sudo tee /etc/exports.d/nfs.exports
sudo systemctl enable --now nfs-server
sudo exportfs -rav
