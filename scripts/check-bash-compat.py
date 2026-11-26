#!/usr/bin/env python3
"""Compare selected shell behavior against Bash in clean, separate directories."""

import argparse
import subprocess
import tempfile
from pathlib import Path


CASES = {
    "quoted arguments": """printf '<%s>\\n' "$0" "$@" """,
    "arithmetic": """n=7; printf '%s\\n' "$((n * 3 + 2))" """,
    "pipeline": """printf 'abc\\n' | tr a-z A-Z""",
    "pipefail": """set -o pipefail; false | true; printf '%s\\n' "$?" """,
    "errexit": """set -e; false; printf 'unexpected\\n'""",
    "nounset": """set -u; printf '<%s>\\n' "$UNSET_PROBE" """,
    "function": """f() { local x=inside; printf '%s:%s\\n' "$x" "$1"; }; x=outside; f item; printf '%s\\n' "$x" """,
    "for loop": """for x in a b; do printf '%s\\n' "$x"; done""",
    "conditional": """if false; then printf bad; else printf good; fi""",
    "substitution": """v=$(printf 'two words\\n\\n'); printf '<%s>\\n' "$v" """,
    "subshell isolation": """x=before; (x=after; printf '%s\\n' "$x"); printf '%s\\n' "$x" """,
    "heredoc": """name=world; cat <<EOF\nhello $name\nEOF\n""",
    "redirect and fd duplication": """{ printf a; printf b >&2; } >out 2>&1; cat out""",
    "array": """x=(a 'b c'); printf '<%s>\\n' "${x[@]}" """,
    "test bracket": """[[ 'two words' == two* ]] && printf yes""",
    "process substitution": """cat <(printf hello)""",
    "trap": """trap 'printf cleanup' EXIT; exit 3""",
    "nullglob": """shopt -s nullglob; for x in no-pair-*; do printf '%s' "$x"; done""",
    "case": """case 'two words' in two*) printf yes;; *) printf no;; esac""",
    "parameter defaults and trim": """x=abc.txt; printf '%s:%s' "${x%.txt}" "${missing:-fallback}" """,
    "command status and short circuit": """false && printf bad; true || printf bad; exit 37""",
    "background wait": """(exit 7) & wait "$!"; printf '%s' "$?" """,
}
def run(command, script, env):
    with tempfile.TemporaryDirectory(prefix="zrun-bash-compat-") as root:
        result = subprocess.run(
            [*command, "-c", script, "fixture", "a b", "c"],
            cwd=root,
            env=dict(env, HOME=root),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=5,
            check=False,
        )
    return result.returncode, result.stdout, result.stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("zrun", type=lambda s: str(Path(s).resolve()))
    parser.add_argument("--bash", default="/usr/bin/bash")
    parser.add_argument("--require", choices=["native", "backend"],
                        help="Fail when the selected route differs from Bash")
    args = parser.parse_args()
    bash = str(Path(args.bash).resolve())
    bash_options = ["--noprofile", "--norc"]
    env = dict(PATH="/usr/bin:/bin", LC_ALL="C", ZRUN_NO_RC="1", ZRUN_SHELL=bash)
    modes = {
        "native": [args.zrun],
        "backend": [args.zrun, "--shell", *bash_options],
    }
    passes = dict.fromkeys(modes, 0)
    for name, script in CASES.items():
        reference = run([bash, *bash_options], script, env)
        for mode, command in modes.items():
            actual = run(command, script, env)
            if actual == reference:
                passes[mode] += 1
            else:
                print(f"{mode}: {name} differs")
                print(f"  Bash: {reference!r}")
                print(f"  zrun: {actual!r}")
    for mode, count in passes.items():
        print(f"{mode}: {count}/{len(CASES)} matched Bash")
    return int(args.require is not None and passes[args.require] != len(CASES))


if __name__ == "__main__":
    raise SystemExit(main())
