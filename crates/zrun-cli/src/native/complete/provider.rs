//! Bridge to locally installed command completion definitions.
use super::{State, display_safe, executable};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    process::{Command, Stdio},
};

// Fish uses the command line as completion input. Quote each decoded word so
// zrun constructs only a literal command line, without fish substitutions.
fn push_word(line: &mut String, word: &str) {
    if !line.is_empty() {
        line.push(' ');
    }
    line.push('\'');
    for c in word.chars() {
        if matches!(c, '\'' | '\\') {
            line.push('\\');
        }
        line.push(c);
    }
    line.push('\'');
}

pub(super) enum Candidates {
    Unavailable,
    Unregistered,
    Provided(BTreeSet<String>),
}

pub(super) fn candidates(state: &State, words: &[String], prefix: &str) -> Candidates {
    let Some(path) = state.env.get(OsStr::new("PATH")) else {
        return Candidates::Unavailable;
    };
    let Some(fish) = std::env::split_paths(path)
        .map(|dir| state.cwd.join(dir).join("fish"))
        .find(|path| path.is_file() && executable(path))
    else {
        return Candidates::Unavailable;
    };
    let command = words[0].rsplit('/').next().unwrap_or_default();
    if command.is_empty() || words.iter().any(|word| !display_safe(word)) {
        return Candidates::Provided(BTreeSet::new());
    }

    let mut line = String::new();
    for word in words {
        push_word(&mut line, word);
    }
    if prefix.is_empty() {
        line.push(' ');
    } else {
        push_word(&mut line, prefix);
    }

    let script = r#"
        if not set -q fish_complete_path
            set -g fish_complete_path $__fish_config_dir/completions
            set -a fish_complete_path $__fish_sysconf_dir/completions
            set -a fish_complete_path $__fish_user_data_dir/vendor_completions.d
            set -a fish_complete_path $__fish_data_dir/vendor_completions.d $__fish_data_dir/completions
        end
        set -l matches (complete -C "$argv[1]")
        if complete -c "$argv[2]" | string match -q '*'
            printf '%s\n' $matches
        else
            exit 3
        end
    "#;
    let output = Command::new("/usr/bin/timeout")
        .args(["--signal=KILL", "0.75s"])
        .arg(fish)
        .args(["--no-config", "-c", script, "--", &line, command])
        .current_dir(&state.cwd)
        .env_clear()
        .envs(&state.env)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Candidates::Provided(BTreeSet::new());
    };
    if output.status.code() == Some(3) {
        return Candidates::Unregistered;
    }
    let mut matches = BTreeSet::new();
    if output.status.success()
        && let Ok(stdout) = std::str::from_utf8(&output.stdout)
    {
        for row in stdout.lines() {
            let value = row.split_once('\t').map_or(row, |(value, _)| value);
            if !value.is_empty() && display_safe(value) {
                matches.insert(value.to_owned());
            }
        }
    }
    Candidates::Provided(matches)
}
