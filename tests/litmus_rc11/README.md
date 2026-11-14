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

`reference.txt` has 20,743 programs:

- every program of two threads with two operations each over two variables,
  using relaxed and `SeqCst` loads, stores and exchanges and `SeqCst` fences;
- random samples of larger shapes of those: up to four threads, three
  operations per thread, three variables, and acquire / release;
- 1,500 programs with strong compare-exchange, with every pair of orders in
  which the failure order is no stronger than the success order;
- 599 programs of two threads with up to five operations;
- 600 programs of five and six threads;
- 97 programs with seven to nine stores to a single variable, which is more
  than loom keeps by default.

An optimized build of the test checks all of them in about ten seconds; an
unoptimized one checks one in eight.

Not covered: weak compare-exchange (herd has none), fences other than
`SeqCst`, and everything that is not an atomic. For the latter see
`tests/exhaustive.rs`, which compares loom with a search of its own that
leaves nothing out.

205 of the generated programs are missing because herd did not finish them
in time: 203 of the 300 with many stores to one variable and one other
within five minutes, one within fifty.

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
7 by default). About a hundred programs here make more stores than that to
one variable, and loom agrees with the reference on them. For the 203 that
herd did not finish, loom with the default gives what it gives with every
store kept, except for one program that it gives up on with a message that
says to keep more stores. Run with `LOOM_ATOMIC_HISTORY=3` on the first
17,947 programs, many of which then make more stores than are kept, loom
misses
allowed outcomes for 45 programs (the stores that would be read are gone),
gives up on 29 with a message that says to keep more stores, and still
finds outcomes that are not allowed for 1. With 5 stores kept it agrees
with the reference on all of them.

## Regenerating

This needs `herd7` (herdtools7; build `herd/herd.exe` with dune) and its
`libdir`, with the modified model saved there as `rc11_cxx20.cat`:

    git clone --depth 1 https://github.com/herd/herdtools7.git
    cd herdtools7
    sed 's/^let rs = .*/let rs = [W \& (RLX | REL | ACQ_REL | ACQ | SC)]; (rf; myrmw)*/' \
        herd/libdir/rc11.cat > herd/libdir/rc11_cxx20.cat
    nix-shell -p ocaml dune_3 ocamlPackages.menhir ocamlPackages.menhirLib \
        ocamlPackages.zarith ocamlPackages.logs ocamlPackages.findlib gnumake \
        --run 'sh ./version-gen.sh $PWD/prefix && dune build herd/herd.exe'

Herd's output takes hours to produce. Keep it, and the build, somewhere that
is not cleaned up behind your back; a build directory is not such a place.

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
