#!/usr/bin/env bash

export SOONG_UI_NINJA_ARGS='-j4'
export USE_CCACHE=1
export CCACHE_EXEC=/usr/bin/ccache

source build/envsetup.sh
breakfast Spacewar
brunch Spacewar
