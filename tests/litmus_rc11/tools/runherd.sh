#!/bin/bash
# runherd.sh HERD_SOURCE_DIR NAME [SECONDS]
#
# Runs herd on lit/NAME.litmus with the modified RC11 model, writing its
# output to hout/NAME.txt. HERD_SOURCE_DIR is a herdtools7 checkout in which
# herd has been built and `herd/libdir/rc11_cxx20.cat` created (see the
# README). A program that takes longer than SECONDS (300 unless given) is
# marked as such in the output file, and left out of the reference.
#
# To run all of them: cut -d' ' -f1 index.txt | xargs -P 16 -n 1 runherd.sh DIR
herd=$1
name=$2
limit=${3:-300}

mkdir -p hout

timeout "$limit" "$herd/_build/default/herd/herd.exe" -I "$herd/herd/libdir" \
    -model rc11_cxx20.cat "lit/$name.litmus" > "hout/$name.txt" 2>&1 \
    || echo "FAILED-OR-TIMEOUT $?" >> "hout/$name.txt"
