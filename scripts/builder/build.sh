#!/usr/bin/env bash

set -x

export SOONG_UI_NINJA_ARGS='-j4'
export USE_CCACHE=1
export CCACHE_EXEC=/usr/bin/ccache

cd lineage
source build/envsetup.sh
breakfast Spacewar
brunch Spacewar
