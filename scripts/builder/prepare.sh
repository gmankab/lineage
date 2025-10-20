#!/usr/bin/env bash

set -x

export PATH="$HOME/.local/bin:$PATH"

git config --global trailer.changeid.key "Change-Id"
git config --global user.email "you@example.com"
git config --global user.name "Your Name"
git config --global color.ui auto
git lfs install

ccache -M 50G
ccache -o compression=true

script -c 'repo init -u https://github.com/LineageOS/android.git -b lineage-23.0 --git-lfs --depth=1' /dev/null
script -c 'repo sync --current-branch --force-sync -j16' /dev/null
script -c 'repo sync --current-branch --force-sync -j1 --fail-fast' /dev/null
git clone --depth=1 -b lineage-22.2 https://github.com/TheMuppets/proprietary_vendor_nothing_Spacewar vendor/nothing/Spacewar
