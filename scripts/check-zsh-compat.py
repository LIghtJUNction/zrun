#!/usr/bin/env python3
"""Compare selected native zrun behavior to clean, noninteractive Zsh."""

import argparse
import subprocess
import tempfile
from pathlib import Path


CASES = {
    "quoted arguments": """printf '<%s>\\n' "$0" "$@" """,
    "pipeline": """printf abc | tr a-z A-Z""",
    "err_exit": """setopt ERR_EXIT; false; printf bad""",
    "unset err_exit": """setopt err_exit; unsetopt ErR_eXiT; false; printf alive""",
    "err_exit condition": """setopt ERR_EXIT; false && printf bad; printf alive""",
    "pipe_fail": """setopt PIPE_FAIL; false | true; printf '%s' "$?" """,
    "no_pipe_fail": """setopt PIPE_FAIL No_PiPe_FaIl; false | true; printf '%s' "$?" """,
    "unset no_pipe_fail": """unsetopt No_Pipe_Fail; false | true; printf '%s' "$?" """,
    "err_exit with pipe_fail": """setopt ERR_EXIT PIPE_FAIL; false | true; printf bad""",
    "allexport": """setopt ALL_EXPORT; EXPORT_PROBE=visible; sh -c 'printf "%s" "$EXPORT_PROBE"' """,
    "no_unset": """setopt NO_UNSET; printf '%s' "$UNSET_PROBE" """,
    "unset enabled": """setopt NO_UNSET; setopt UNSET; printf '<%s>' "$UNSET_PROBE" """,
    "default under no_unset": """setopt NO_UNSET; printf '%s' "${UNSET_PROBE:-fallback}" """,
    "unknown option": """setopt INVALID_FIXTURE_OPTION PIPE_FAIL; printf '%s:' "$?"; false | true; printf '%s' "$?" """,
    "function": """f() { local arg="$1"; printf '<%s>' "$arg"; }; f 'two words'""",
    "for loop": """for item in a b; do printf '%s' "$item"; done""",
    "command substitution": """out=$(printf 'two words\\n'); printf '<%s>' "$out" """,
    "one-based arrays": """x=(one two); printf '%s' "$x[1]" """,
    "extended glob": """setopt EXTENDED_GLOB; [[ x123 = x[0-9]## ]] && printf yes""",
    "here-document": """name=world; cat <<EOF\nhello $name\nEOF\n""",
}


def run(command, script):
    with tempfile.TemporaryDirectory(prefix="zrun-zsh-compat-") as root:
        result = subprocess.run(
            [*command, "-c", script, "fixture", "a b", "c"],
            cwd=root,
            env=dict(PATH="/usr/bin:/bin", LC_ALL="C", HOME=root, ZDOTDIR=root, ZRUN_NO_RC="1"),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=5,
            check=False,
        )
    return result.returncode, result.stdout, result.stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("zrun", type=lambda path: str(Path(path).resolve()))
    parser.add_argument("--zsh", default="/usr/bin/zsh")
    parser.add_argument("--require-native", action="store_true",
                        help="Fail if any native behavior differs from Zsh")
    args = parser.parse_args()
    matches = 0
    for name, script in CASES.items():
        reference = run([str(Path(args.zsh).resolve()), "-f"], script)
        native = run([args.zrun], script)
        if native == reference:
            matches += 1
        else:
            print(f"{name} differs\n  Zsh:  {reference!r}\n  zrun: {native!r}")
    print(f"native: {matches}/{len(CASES)} matched Zsh")
    return int(args.require_native and matches != len(CASES))


if __name__ == "__main__":
    raise SystemExit(main())
