#!/usr/bin/env bash

set -x

export SOONG_UI_NINJA_ARGS='-j4'
export USE_CCACHE=0

cd lineage
source build/envsetup.sh
breakfast Spacewar
brunch Spacewar
