#!/usr/bin/env python3
"""Compare zrun's native registrations with the local Zsh builtin table."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("zrun", type=Path, help="candidate zrun binary")
    parser.add_argument("--zsh", default="zsh", help="reference Zsh binary")
    args = parser.parse_args()
    reference = shutil.which(args.zsh)
    if reference is None:
        parser.error(f"reference shell unavailable: {args.zsh}")
    binary = args.zrun.resolve(strict=True)
    with tempfile.TemporaryDirectory() as root:
        env = dict(os.environ, HOME=root, ZDOTDIR=root, LC_ALL="C")
        names = subprocess.check_output(
            [reference, "-fc", "print -rl -- ${(ok)builtins}"],
            cwd=root, env=env, text=True,
        ).splitlines()
        env.update(PATH="", ZRUN_NO_RC="1", ZRUN_SHELL="/missing-reference-shell")
        result = subprocess.run(
            [binary, "-c", 'whence -w -- "$@"', "fixture", *names],
            cwd=root, env=env, text=True, capture_output=True,
        )
    if result.returncode not in (0, 1) or result.stderr:
        raise RuntimeError(f"native lookup failed ({result.returncode}): {result.stderr}")
    records = {}
    for line in result.stdout.splitlines():
        name, separator, classification = line.rpartition(": ")
        if not separator or name in records:
            raise RuntimeError(f"unexpected lookup output: {line!r}")
        records[name] = classification
    if set(records) != set(names):
        raise RuntimeError("native lookup returned a different set of command names")
    missing = [name for name in names if records[name] != "builtin"]
    print(f"Registered {len(names)-len(missing)} of {len(names)} reference builtin names")
    if missing:
        print("Missing:", " ".join(missing))
    return int(bool(missing))


if __name__ == "__main__":
    sys.exit(main())
