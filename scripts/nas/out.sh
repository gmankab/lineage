#!/usr/bin/env bash

set -uexo pipefail

sudo mkdir -p /etc/exports.d
sudo mkdir -p export/out
sudo chmod 777 export/out
echo "$(pwd)/export $builder(rw,async,fsid=0)" | sudo tee /etc/exports.d/nfs.exports
sudo systemctl enable --now nfs-server
sudo exportfs -rav

export dir=$(pwd)/export/out
bash scripts/nas/wait.sh
