# Litmus tests against a reference memory model

There are two references: `reference.txt`, computed by the [herd7] simulator
from a formal model, and `reference_genmc.txt`, computed by the [GenMC] model
checker, which is fast and knows locks. Most of this file is about the first;
see "GenMC" at the end for the second.

`reference.txt` holds small concurrent programs and, for each, the outcomes the
RC11 memory model allows, as computed by herd. The test in
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

`reference.txt` has 20,747 programs:

- every program of two threads with two operations each over two variables,
  using relaxed and `SeqCst` loads, stores and exchanges and `SeqCst` fences;
- random samples of larger shapes of those: up to four threads, three
  operations per thread, three variables, and acquire / release;
- 1,500 programs with strong compare-exchange, with every pair of orders in
  which the failure order is no stronger than the success order;
- 599 programs of two threads with up to five operations;
- 600 programs of five and six threads;
- 97 programs with seven to nine stores to a single variable, which is more
  than loom keeps by default;
- 4 larger programs on which loom and GenMC disagreed, see below.

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

## GenMC

GenMC explores the executions of a C program under RC11, one execution per
equivalence class, and runs each to its end. `tools/genmc.py` turns a litmus
program into a C program that prints what it read, runs GenMC on it, and
collects what was printed. All 20,948 programs of the herd corpus take it a
minute and a half, where herd takes hours and does not finish 205.

On the 20,747 programs that have a herd result, GenMC finds the same outcomes
for all but 4. For 3 of those it finds outcomes that herd does not allow,
and for 1 it misses outcomes that herd allows.
Loom agrees with herd on all 4. So GenMC is a second opinion and not a
ground truth, and a disagreement with it is settled by herd where herd can
be asked.

`reference_genmc.txt` holds 2,968 programs that herd cannot be given, or
does not finish, and that loom explores in under a third of a second each:

- 1,496 programs with locks (`La`, `Ua`, `Lb`, `Ub`: two mutexes, never taken
  in an order that can deadlock) around the same operations as before;
- 708 larger programs: three and four threads of up to four operations, two
  threads of up to six;
- 375 with compare-exchange and 272 with many stores to one variable, of
  those larger shapes;
- 117 of the programs that herd did not finish.

Loom finds exactly what GenMC finds for all of them. Of the 3,905 programs
GenMC was given in all, loom finishes 3,895 within 45 seconds each. It
agrees with GenMC on 3,889 of those. The others are 3 of the 4 programs
above, which herd decides in loom's favor (they are in `reference.txt`,
with herd's result, along with a fourth that showed a bug in loom), and 3
that loom gives up on, saying that more stores have to be kept.

To build GenMC (v0.20.0 needs LLVM 19 to 22; it looks for `clang` next to
the LLVM tools only, where nix does not put it, hence the `sed`):

    git clone --depth 1 https://github.com/MPI-SWS/genmc.git
    cd genmc
    sed -i 's/ NO_CACHE NO_DEFAULT_PATH REQUIRED)/ NO_CACHE REQUIRED)/' CMakeLists.txt
    nix-shell -p cmake gnumake llvmPackages_19.llvm llvmPackages_19.llvm.dev \
        llvmPackages_19.clang libffi zlib libedit ncurses libxml2 \
        --run 'cmake -DCMAKE_BUILD_TYPE=Release -B build -S . && cmake --build build -j'

To run it: `python3 tools/genmc.py genmc/build/bin/genmc < programs.txt >
results.txt` writes lines in the format of the reference files. `gen.py`
writes the programs for GenMC after the ones for herd; `gen.py 20948` writes
only those.

[herd7]: https://github.com/herd/herdtools7
[GenMC]: https://github.com/MPI-SWS/genmc
