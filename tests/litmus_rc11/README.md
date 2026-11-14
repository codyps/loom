# Litmus tests against a reference memory model

`reference.txt` holds small concurrent programs and, for each, the outcomes the
RC11 memory model allows, as computed by the [herd7] simulator. The test in
`tests/litmus_reference.rs` runs every program under loom and checks that loom
finds no outcome that is not allowed, and all the allowed ones except those
recorded as known gaps. `mod.rs` documents the program notation and contains
the interpreter that runs a program under loom.

The reference model is herd's `rc11.cat` ("Repairing Sequential Consistency in
C/C++11", Lahav et al., PLDI 2017) with one change: release sequences are the
C++20 ones, which is what Rust has. In `rc11.cat` that is the single line

    let rs = [W & (RLX | REL | ACQ_REL | ACQ | SC)]; (rf; myrmw)*

instead of the C++11 definition, in which a later store by the same thread
continues a release sequence.

## What is covered

`reference.txt` has 17,946 programs: every program of two threads with two
operations each over two variables, using relaxed and `SeqCst` loads, stores
and exchanges and `SeqCst` fences, plus random samples of larger shapes (up to
four threads, three operations per thread, three variables, acquire /
release). An optimized build of the test checks all of them in a few
seconds; an unoptimized one checks one in eight.

Not covered: `compare_exchange`, fences other than `SeqCst`, more than four
threads or three operations per thread, and everything that is not an atomic.
For the latter see `tests/exhaustive.rs`, which compares loom with a search
of its own that leaves nothing out.

Two of the generated programs are missing because herd did not finish them
within 50 minutes.

## Known gaps

None against this reference: loom finds exactly the outcomes the model allows
for every program, with the default settings.

The reference format still has room for them. A third field on a line of
`reference.txt` lists allowed outcomes loom is known not to find; the test
then requires those to stay missing, so that both a regression and an
improvement are noticed.

RC11 forbids load buffering (a load observing a store that is executed later),
which C++ and Rust allow in principle. Loom cannot produce it either, so the
comparison says nothing about it.

Loom keeps a limited number of stores per atomic (`Builder::atomic_history`,
7 by default). Two programs here make more stores than that to one variable.
Run with `LOOM_ATOMIC_HISTORY=3`, where most programs do, loom misses
allowed outcomes for 46 programs (the stores that would be read are gone),
gives up on 16 with a message that says to keep more stores, and still
finds outcomes that are not allowed for 2.

## Regenerating

This needs `herd7` (herdtools7; build `herd/herd.exe` with dune) and its
`libdir`, with the modified model saved there as `rc11_cxx20.cat`.

1. `python3 tools/gen.py` writes the programs to `programs.txt` (one per
   line), an index to `index.txt`, and a C litmus file per program to `lit/`.
2. Run herd on each: `cut -d' ' -f1 index.txt | xargs -P 16 -n 1
   tools/runherd.sh <herdtools7>/` writes `hout/<name>.txt`. With the default
   limit of 5 minutes per program this takes a few hours of CPU time, and
   leaves out a handful of the largest programs; `runherd.sh <dir> <name>
   3000` into another directory gets most of those.
3. `python3 tools/reference.py index.txt hout [more directories] >
   reference.txt` turns herd's output into the reference.
4. `cargo test --release --test litmus_reference` checks loom against it.

To look at loom's outcomes directly, or to run with a time limit per
program: `LOOM_LITMUS_IN=programs.txt LOOM_LITMUS_OUT=lout/p00
LOOM_LITMUS_BUDGET=5 cargo test --release --test litmus_gen`, then `python3
tools/compare.py lout` (which expects `index.txt` and `hout/` in the current
directory) prints the totals, and writes the programs for which loom found a
forbidden outcome to `lout/extra.txt` and those for which it missed one to
`lout/missing.txt`. `LOOM_LITMUS_PREEMPTIONS` sets a preemption bound and
`LOOM_LITMUS_EXHAUSTIVE` makes the search exhaustive, for comparing the two.

[herd7]: https://github.com/herd/herdtools7
