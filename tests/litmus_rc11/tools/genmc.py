#!/usr/bin/env python3
"""Runs litmus programs under GenMC and prints the outcomes it finds.

Usage: genmc.py GENMC [SECONDS] < programs > results

GENMC is the `genmc` executable. Programs are read one per line, in the
notation of `mod.rs`; for each, a line in the format of `reference.txt` is
written (`program | outcomes`), or `program ! reason` if GenMC failed or
took longer than SECONDS (60 unless given).

GenMC explores each execution of a C program once and runs it to its end,
so a program that prints what it read at the end prints every outcome.
"""
import os
import re
import subprocess
import sys
import tempfile

ORDER = {"r": "memory_order_relaxed", "s": "memory_order_seq_cst"}


def order(o, kind):
    if o == "a":
        return {
            "W": "memory_order_release",
            "R": "memory_order_acquire",
            "X": "memory_order_acq_rel",
        }[kind]
    return ORDER[o]


def c_program(program):
    threads = program.split(";")
    out = [
        "#include <stdatomic.h>",
        "#include <pthread.h>",
        "#include <stdio.h>",
        "",
        "atomic_int x, y, z;",
        "pthread_mutex_t a = PTHREAD_MUTEX_INITIALIZER, b = PTHREAD_MUTEX_INITIALIZER;",
    ]
    reads = []

    for t, ops in enumerate(threads):
        body = []
        n = 0

        for op in ops.split(","):
            op = op.strip()
            reg = "r%d_%d" % (t, n)

            if op[0] == "W":
                body.append(
                    "  atomic_store_explicit(&%s, %s, %s);" % (op[1], op[2], order(op[3], "W"))
                )
            elif op[0] == "R":
                body.append(
                    "  %s = atomic_load_explicit(&%s, %s);" % (reg, op[1], order(op[2], "R"))
                )
                n += 1
            elif op[0] == "X":
                body.append(
                    "  %s = atomic_exchange_explicit(&%s, %s, %s);"
                    % (reg, op[1], op[2], order(op[3], "X"))
                )
                n += 1
            elif op[0] == "C":
                found = "r%d_%d" % (t, n + 1)
                body.append("  %s = %s;" % (found, op[2]))
                body.append(
                    "  %s = atomic_compare_exchange_strong_explicit(&%s, &%s, %s, %s, %s);"
                    % (reg, op[1], found, op[3], order(op[4], "X"), order(op[5], "R"))
                )
                n += 2
            elif op[0] == "F":
                body.append("  atomic_thread_fence(memory_order_seq_cst);")
            elif op[0] == "L":
                body.append("  pthread_mutex_lock(&%s);" % op[1])
            elif op[0] == "U":
                body.append("  pthread_mutex_unlock(&%s);" % op[1])
            else:
                raise ValueError(op)

        regs = ["r%d_%d" % (t, i) for i in range(n)]
        reads.append(regs)

        if regs:
            out.append("int %s;" % ", ".join(regs))

        out.append("void *thread%d(void *arg) {" % t)
        out.extend(body)
        out.append("  return NULL;")
        out.append("}")
        out.append("")

    out.append("int main() {")
    out.append("  pthread_t t[%d];" % len(threads))

    for t in range(len(threads)):
        out.append("  pthread_create(&t[%d], NULL, thread%d, NULL);" % (t, t))

    for t in range(len(threads)):
        out.append("  pthread_join(t[%d], NULL);" % t)

    fmt = "".join("%d:%s|" % (t, ",".join("%d" for _ in regs)) for t, regs in enumerate(reads))
    args = [reg for regs in reads for reg in regs]
    args += ["atomic_load(&%s)" % v for v in "xyz"]
    out.append('  printf("OUT %sx=%%d,y=%%d,z=%%d\\n", %s);' % (fmt, ", ".join(args)))
    out.append("  return 0;")
    out.append("}")

    return "\n".join(out) + "\n"


# GenMC skips work that cannot change whether a program is correct. Whether
# it could change what a program prints is another matter, so everything of
# the kind is turned off.
FLAGS = [
    "--rc11",
    "--disable-estimation",
    "--disable-sr",
    "--disable-ipr",
    "--disable-final-write",
    "--disable-helper",
    "--disable-bam",
    "--disable-load-annotation",
    "--disable-assume-propagation",
    "--disable-spin-assume",
    "--disable-code-condenser",
    # Anything GenMC has to say would end up between the lines the program
    # prints.
    "--v0",
]


def run(genmc, program, seconds, directory):
    path = os.path.join(directory, "p.c")

    with open(path, "w") as f:
        f.write(c_program(program))

    try:
        result = subprocess.run(
            [genmc] + FLAGS + [path],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=seconds,
            text=True,
        )
    except subprocess.TimeoutExpired:
        return None, "timeout"

    if result.returncode != 0:
        lines = (result.stdout + result.stderr).strip().splitlines()
        return None, "exit %d: %s" % (result.returncode, lines[-1] if lines else "")

    outcomes = set()

    for line in result.stdout.splitlines():
        if not line.startswith("OUT "):
            continue

        if not re.fullmatch(r"OUT (\d+:[\d,]*\|)+x=\d+,y=\d+,z=\d+", line):
            return None, "garbled output: %s" % line

        outcomes.add(line[4:])

    return outcomes, None


def main():
    genmc = sys.argv[1]
    seconds = int(sys.argv[2]) if len(sys.argv) > 2 else 60

    with tempfile.TemporaryDirectory() as directory:
        for line in sys.stdin:
            program = line.strip()

            if not program:
                continue

            outcomes, error = run(genmc, program, seconds, directory)

            if outcomes is None:
                print("%s ! %s" % (program, error))
            else:
                print("%s | %s" % (program, " ".join(sorted(outcomes))))

            sys.stdout.flush()


if __name__ == "__main__":
    main()
