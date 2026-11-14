#!/usr/bin/env python3
"""Builds `reference.txt` from herd's output.

Usage: reference.py INDEX HERD_OUTPUT_DIR... > reference.txt

INDEX is the `index.txt` written by `gen.py`: a name, a tag and a program per
line. Each directory holds herd's output for the programs, as `<name>.txt`;
for a program that has a result in more than one of them, the first
directory wins. Programs without a result (herd ran out of time) are left
out, and counted on stderr.
"""
import collections
import re
import sys


def outcomes(path, program):
    try:
        text = open(path).read()
    except FileNotFoundError:
        return None

    if "FAILED-OR-TIMEOUT" in text or "States" not in text:
        return None

    threads = program.count(";") + 1
    found = set()

    for line in text.splitlines():
        line = line.strip()

        if not re.match(r"^(\d+:r\d+=|\[[xyz]\]=)", line):
            continue

        registers = collections.defaultdict(dict)
        memory = {}

        for item in line.rstrip(";").split(";"):
            item = item.strip()

            m = re.match(r"(\d+):r(\d+)=(\d+)", item)
            if m:
                registers[int(m.group(1))][int(m.group(2))] = m.group(3)
                continue

            m = re.match(r"\[([xyz])\]=(\d+)", item)
            if m:
                memory[m.group(1)] = m.group(2)

        outcome = "".join(
            "%d:%s|" % (t, ",".join(registers[t][r] for r in sorted(registers[t])))
            for t in range(threads)
        )
        outcome += "x=%s,y=%s,z=%s" % (memory["x"], memory["y"], memory["z"])
        found.add(outcome)

    return found


def main():
    index, directories = sys.argv[1], sys.argv[2:]
    missing = 0

    for line in open(index).read().splitlines():
        name, _tag, program = line.split(" ", 2)

        for directory in directories:
            allowed = outcomes("%s/%s.txt" % (directory, name), program)
            if allowed is not None:
                break

        if allowed is None:
            missing += 1
            continue

        print("%s | %s" % (program, " ".join(sorted(allowed))))

    print("%d programs without a result" % missing, file=sys.stderr)


if __name__ == "__main__":
    main()
