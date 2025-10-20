#!/usr/bin/env bash

set -uexo pipefail

sudo apt update
sudo apt install yggdrasil nfs-common

mkdir lineage
