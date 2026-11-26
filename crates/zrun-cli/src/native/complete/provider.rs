//! Consult installed Bash programmable completion functions when available.
use super::{State, display_safe, executable};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::Path,
    process::{Command, Stdio},
};

pub(super) fn candidates(
    state: &State,
    words: &[String],
    prefix: &str,
) -> Option<BTreeSet<String>> {
    let library = Path::new("/usr/share/bash-completion/bash_completion");
    if !library.is_file() || words.iter().any(|word| !display_safe(word)) {
        return None;
    }
    let bash = std::env::split_paths(state.env.get(OsStr::new("PATH"))?)
        .map(|dir| state.cwd.join(dir).join("bash"))
        .find(|path| path.is_file() && executable(path))?;

    // Pass words as data. Only installed completion scripts are sourced.
    // Bash's default filename fallback is deliberately never invoked.
    let script = r#"
        source "$1" >&2 || exit 3
        shift
        command_name=${1##*/}
        [[ -n $command_name ]] || exit 3
        complete -p -- "$command_name" >/dev/null 2>&1 ||
            { declare -F _comp_load >/dev/null && _comp_load "$command_name" >&2; } ||
            { declare -F _completion_loader >/dev/null && _completion_loader "$command_name" >&2; }
        spec=$(complete -p -- "$command_name") || exit 3
        [[ $spec =~ [[:space:]]-F[[:space:]]([a-zA-Z_][a-zA-Z_0-9]*)[[:space:]] ]] || exit 3
        handler=${BASH_REMATCH[1]}
        COMP_WORDS=("$@")
        COMP_CWORD=$(($# - 1))
        COMP_TYPE=9 COMP_KEY=9
        COMP_LINE=
        for ((i=0; i<COMP_CWORD; ++i)); do
            printf -v COMP_LINE '%s%q ' "$COMP_LINE" "${COMP_WORDS[i]}"
        done
        current=${COMP_WORDS[COMP_CWORD]}
        if [[ -n $current ]]; then
            printf -v COMP_LINE '%s%q' "$COMP_LINE" "$current"
        fi
        LC_ALL=C
        COMP_POINT=${#COMP_LINE}
        COMPREPLY=()
        "$handler" "$1" "$current" "${COMP_WORDS[COMP_CWORD-1]}" >&2
        if (("${#COMPREPLY[@]}")); then
            printf '%s\0' "${COMPREPLY[@]}"
        fi
    "#;
    let output = Command::new("/usr/bin/timeout")
        .args(["--signal=KILL", "2s"])
        .arg(bash)
        .args(["--noprofile", "--norc", "-c", script, "zrun-completion"])
        .arg(library)
        .args(words)
        .arg(prefix)
        .current_dir(&state.cwd)
        .env_clear()
        .envs(
            state
                .env
                .iter()
                .filter(|(key, _)| !key.as_encoded_bytes().starts_with(b"BASH_FUNC_")),
        )
        .env_remove("BASH_ENV")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.code() == Some(3) {
        return None;
    }
    let mut matches = BTreeSet::new();
    if output.status.success()
        && let Ok(stdout) = std::str::from_utf8(&output.stdout)
    {
        matches.extend(
            stdout
                .split('\0')
                .filter(|value| !value.is_empty() && display_safe(value))
                .map(str::to_owned),
        );
    }
    Some(matches)
}
