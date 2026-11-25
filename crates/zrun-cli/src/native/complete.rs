//! Literal completion for unfinished native shell input. Never evaluates a command or expansion.
mod branches;
mod commands;

use super::{execute::State, syntax};
use commands::Group;
use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr},
    fs,
    ops::Range,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub(super) struct Completion {
    pub(super) replace: Option<(Range<usize>, String)>,
    pub(super) candidates: Vec<String>,
}

const BUILTINS: &[&str] = &[
    ":", "ai", "alias", "async", "await", "cd", "command", "exec", "exit", "export", "false",
    "pwd", "shell", "true", "unalias", "unset",
];

#[derive(Clone, Copy, PartialEq)]
enum Position {
    Command,
    Argument,
    Directory,
    AsyncName,
    Marker,
    Group(Group),
    GlobalValue(Group, bool),
    Options(Group, &'static str),
}

struct Target<'a> {
    word: &'a str,
    range: Range<usize>,
    prefix: String,
    position: Position,
}

// Unlike the execution parser, editing must accept a trailing escape or unclosed quote.
// The consumed length includes quotes and escapes; the lookup prefix is their literal value.
fn word(input: &str) -> (usize, String, bool) {
    let mut chars = input.char_indices().peekable();
    let mut value = String::new();
    let mut quote = None;
    let mut expansion = false;
    while let Some((i, c)) = chars.next() {
        if quote == Some(c) {
            quote = None;
        } else if quote != Some('\'') && c == '\\' {
            if let Some((_, next)) = chars.next() {
                if quote == Some('"') && !matches!(next, '$' | '"' | '\\' | '`' | '\n') {
                    value.push('\\');
                }
                if next != '\n' {
                    value.push(next);
                }
            }
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if quote.is_none() && (c.is_whitespace() || "|;&<>".contains(c)) {
            return (i, value, expansion);
        } else {
            expansion |= quote != Some('\'') && matches!(c, '$' | '`');
            value.push(c);
        }
    }
    (input.len(), value, expansion)
}

fn next_position(previous: Position, raw: &str, value: &str) -> Position {
    match previous {
        Position::Command => match value {
            "cd" => Position::Directory,
            "async" => Position::AsyncName,
            "shell" => Position::Marker,
            "command" | "exec" => Position::Command,
            _ if raw
                .split_once('=')
                .is_some_and(|(name, _)| syntax::identifier(name)) =>
            {
                Position::Command
            }
            _ => commands::root(value).map_or(Position::Argument, Position::Group),
        },
        Position::Group(group) => group.after(value),
        Position::GlobalValue(group, _) => Position::Group(group),
        Position::Options(_, _) if value == "--" => Position::Argument,
        Position::AsyncName => Position::Marker,
        Position::Marker if value == "--" => Position::Command,
        pos => pos,
    }
}

// Longest operators first. Redirections consume a path without changing command position.
fn operator(input: &str) -> Option<(&'static str, bool)> {
    [
        "2>&1", "1>&2", ">&2", "2>>", "1>>", ">>", "2>", "1>", "0<", ">", "<", "&&", "||", "|",
        ";", "&",
    ]
    .into_iter()
    .find(|op| input.starts_with(op))
    .map(|op| (op, op.contains('>') || op.contains('<')))
}

fn target(line: &str, cursor: usize) -> Option<Target<'_>> {
    if !line.is_char_boundary(cursor) {
        return None;
    }
    let mut i = 0;
    let mut position = Position::Command;
    let mut redirect = false;
    let mut other_repo = false;
    while i < line.len() {
        let tail = &line[i..];
        let c = tail.chars().next()?;
        if c == '#' && cursor >= i {
            return None;
        }
        if c.is_whitespace() {
            if cursor == i {
                break;
            }
            if c == '\n' {
                position = Position::Command;
                other_repo = false;
                redirect = false;
            }
            i += c.len_utf8();
            continue;
        }
        if let Some((op, is_redirect)) = operator(tail) {
            if cursor == i {
                break;
            }
            i += op.len();
            if cursor < i {
                return None;
            }
            redirect = is_redirect && !op.contains('&');
            if !is_redirect {
                position = Position::Command;
                other_repo = false;
            }
            continue;
        }
        let (length, value, expansion) = word(tail);
        let end = i + length;
        if cursor <= end {
            if expansion {
                return None;
            }
            return Some(Target {
                word: &line[i..end],
                range: i..end,
                prefix: word(&line[i..cursor]).1,
                position: if redirect {
                    Position::Argument
                } else {
                    position
                },
            });
        }
        if redirect {
            redirect = false;
        } else {
            if position == Position::Command
                && value.split_once('=').is_some_and(|(key, _)| {
                    matches!(key, "GIT_DIR" | "GIT_WORK_TREE" | "GIT_COMMON_DIR")
                })
            {
                other_repo = true;
            }
            position = next_position(position, &line[i..end], &value);
            if other_repo && position == Position::Group(Group::Git) {
                position = Position::Group(Group::GitOtherDirectory);
            }
        }
        i = end;
    }
    Some(Target {
        word: "",
        range: cursor..cursor,
        prefix: String::new(),
        position: if redirect {
            Position::Argument
        } else {
            position
        },
    })
}

fn display_safe(s: &str) -> bool {
    !s.chars().any(|c| {
        c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' |
        '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}

fn executable(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: path remains a live NUL-terminated buffer throughout this permission query.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0 }
}

fn directory_entries(
    dir: &Path,
    parent: &str,
    prefix: &str,
    position: Position,
    path_search: bool,
    matches: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if match_quality(prefix, &name).is_none()
            || (!prefix.starts_with('.') && name.starts_with('.'))
            || !display_safe(&name)
        {
            continue;
        }
        // metadata follows executable/dir symlinks without opening FIFOs, devices or file contents.
        let path = entry.path();
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if !path_search {
                matches.insert(format!("{parent}{name}/"));
            }
        } else if metadata.is_file()
            && position != Position::Directory
            && (position != Position::Command || executable(&path))
        {
            matches.insert(format!("{parent}{name}"));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Quality {
    Prefix,
    Substring,
    Subsequence,
}

fn match_quality(query: &str, name: &str) -> Option<Quality> {
    if name.starts_with(query) {
        return Some(Quality::Prefix);
    }
    // A single letter should narrow the menu instead of matching almost all of PATH.
    let mut chars = query.chars();
    chars.next()?;
    chars.next()?;
    if name.contains(query) {
        return Some(Quality::Substring);
    }
    let mut haystack = name.chars();
    query
        .chars()
        .all(|c| haystack.any(|n| n == c))
        .then_some(Quality::Subsequence)
}

fn add_names<'a>(
    matches: &mut BTreeSet<String>,
    names: impl Iterator<Item = &'a str>,
    prefix: &str,
) {
    matches.extend(
        names
            .filter(|name| match_quality(prefix, name).is_some())
            .map(str::to_owned),
    );
}

fn candidates(state: &State, target: &Target<'_>) -> BTreeSet<String> {
    let mut matches = BTreeSet::new();
    let prefix = target.prefix.as_str();
    if !display_safe(prefix) {
        return matches;
    }
    match target.position {
        Position::Group(group) if !prefix.contains('/') => {
            let names = if prefix.starts_with('-') {
                group.flags(None)
            } else {
                group.names()
            };
            add_names(&mut matches, names.split_ascii_whitespace(), prefix);
            if group == Group::Zrun
                && !prefix.is_empty()
                && !prefix.starts_with('-')
                && !matches.iter().any(|name| name.starts_with(prefix))
            {
                add_executables(state, prefix, &mut matches);
            }
        }
        Position::Options(group, leaf) if prefix.starts_with('-') => {
            add_names(
                &mut matches,
                group.flags(Some(leaf)).split_ascii_whitespace(),
                prefix,
            );
        }
        Position::Options(Group::Git, "switch" | "checkout" | "merge" | "rebase" | "branch") => {
            branches::local(state, prefix, &mut matches);
            if matches!(target.position, Position::Options(_, "checkout")) {
                add_paths(state, prefix, Position::Argument, &mut matches);
            }
        }
        Position::GlobalValue(_, true) => {
            add_paths(state, prefix, Position::Directory, &mut matches)
        }
        Position::GlobalValue(_, false) | Position::AsyncName => {}
        Position::Marker => {
            if "--".starts_with(prefix) {
                matches.insert("--".to_string());
            }
        }
        Position::Command if !prefix.contains('/') => {
            add_names(&mut matches, BUILTINS.iter().copied(), prefix);
            add_executables(state, prefix, &mut matches);
        }
        pos => add_paths(state, prefix, pos, &mut matches),
    }
    matches
}

fn add_executables(state: &State, prefix: &str, matches: &mut BTreeSet<String>) {
    if let Some(path) = state.env.get(OsStr::new("PATH")) {
        let mut seen = BTreeSet::<PathBuf>::new();
        for entry in std::env::split_paths(path) {
            let dir = state.cwd.join(entry);
            if seen.insert(dir.clone()) {
                directory_entries(&dir, "", prefix, Position::Command, true, matches);
            }
        }
    }
}

fn add_paths(state: &State, prefix: &str, pos: Position, matches: &mut BTreeSet<String>) {
    let split = prefix.rfind('/').map_or(0, |n| n + 1);
    let (parent, basename) = prefix.split_at(split);
    directory_entries(
        &state.cwd.join(parent),
        parent,
        basename,
        pos,
        false,
        matches,
    );
}

fn quoted(value: &str, original: &str) -> String {
    if original.starts_with('\'') {
        return format!("'{}'", value.replace('\'', "'\\''"));
    }
    let double = original.starts_with('"');
    let mut result = String::new();
    if double {
        result.push('"');
    }
    for c in value.chars() {
        if (double && matches!(c, '"' | '$' | '`' | '\\'))
            || (!double && (c.is_whitespace() || "\\'\"$`|;&<>#()[]{}*!?~".contains(c)))
        {
            result.push('\\');
        }
        result.push(c);
    }
    if double {
        result.push('"');
    }
    result
}

pub(super) fn complete(state: &State, line: &str, cursor: usize) -> Completion {
    let Some(target) = target(line, cursor) else {
        return Completion::default();
    };
    let mut ranked: Vec<_> = candidates(state, &target)
        .into_iter()
        .filter_map(|name| match_quality(&target.prefix, &name).map(|quality| (quality, name)))
        .collect();
    // Within each quality the original BTreeSet order is lexical.
    ranked.sort_by_key(|(quality, _)| *quality);
    let Some(&(best, _)) = ranked.first() else {
        return Completion::default();
    };
    let strong = ranked
        .iter()
        .take_while(|(quality, _)| *quality == best)
        .count();
    if strong == 1 && best == Quality::Prefix {
        ranked.truncate(1);
    }
    let names: Vec<_> = ranked.into_iter().map(|(_, name)| name).collect();
    let replace = if names.len() == 1 {
        select_target(&target, line, &names[0])
    } else if best == Quality::Prefix {
        let first = &names[0];
        let len = first
            .chars()
            .zip(names[strong - 1].chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        (len > target.prefix.len())
            .then(|| (target.range.clone(), quoted(&first[..len], target.word)))
    } else {
        None
    };
    Completion {
        replace: replace.filter(|(_, text)| text != target.word),
        candidates: names,
    }
}

// The editor selects only from its original menu. Serializing the candidate through the
// same word parser/quoting path keeps metacharacters literal and preserves following words.
pub(super) fn select(line: &str, cursor: usize, name: &str) -> Option<(Range<usize>, String)> {
    let target = target(line, cursor)?;
    select_target(&target, line, name)
}

fn select_target(target: &Target<'_>, line: &str, name: &str) -> Option<(Range<usize>, String)> {
    if name.is_empty() || !display_safe(name) {
        return None;
    }
    let mut text = quoted(name, target.word);
    if !name.ends_with('/') && target.range.end == line.len() {
        text.push(' ');
    }
    Some((target.range.clone(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        state: State,
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "zrun-completion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("bin")).unwrap();
            let mut state = State::new().unwrap();
            state.cwd = root.clone();
            state.env.insert("PATH".into(), "bin:bin".into());
            Self { state, root }
        }
        fn file(&self, path: &str, mode: u32) {
            let path = self.root.join(path);
            fs::write(&path, "").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
        fn run(&self, prefix: &str, suffix: &str) -> Completion {
            complete(&self.state, &format!("{prefix}{suffix}"), prefix.len())
        }
        fn apply(&self, prefix: &str, suffix: &str) -> String {
            let mut line = format!("{prefix}{suffix}");
            let (range, text) = self
                .run(prefix, suffix)
                .replace
                .expect("completion changes the input");
            line.replace_range(range, &text);
            line
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn commands_use_session_path_permissions_directories_and_builtins() {
        let f = Fixture::new();
        f.file("bin/bravo", 0o700);
        f.file("bin/bread", 0o600);
        f.file("bin/async", 0o700);
        fs::create_dir(f.root.join("bin/brook")).unwrap();
        symlink("bravo", f.root.join("bin/bright")).unwrap();
        symlink("absent", f.root.join("bin/broken")).unwrap();
        assert_eq!(f.run("br", "").candidates, ["bravo", "bright"]);
        assert_eq!(f.apply("bra", ""), "bravo ");
        assert_eq!(f.run("as", "").candidates, ["async"]);
        assert_eq!(f.apply("aw", ""), "await ");
        f.file("bin/plain", 0o644);
        assert_eq!(f.apply("./bi", ""), "./bin/");
        assert!(f.run("./bin/pl", "").candidates.is_empty());
        assert_eq!(f.apply("cat ./bin/pl", ""), "cat ./bin/plain ");
    }

    #[test]
    fn command_positions_follow_operators_redirections_and_native_prefixes() {
        let f = Fixture::new();
        f.file("bin/dolphin", 0o755);
        f.file("document", 0o644);
        for prefix in [
            "true|dol",
            "false||dol",
            "true && dol",
            "true;dol",
            "exec dol",
            "command dol",
            "async task -- dol",
            "X=$HOME dol",
            "1>out dol",
            "2>&1 dol",
        ] {
            assert_eq!(f.run(prefix, "").candidates, ["dolphin"], "{prefix}");
        }
        for prefix in [
            "true 'literal | ;' do",
            "true \\| do",
            ">do",
            "dolphin 2>do",
            "cat<do",
        ] {
            assert_eq!(f.run(prefix, "").candidates, ["document"], "{prefix}");
        }
        assert!(f.run("echo # dol", "").candidates.is_empty());
        assert_eq!(f.apply("async t -", ""), "async t -- ");
    }

    #[test]
    fn filenames_preserve_spaces_quotes_and_literal_shell_metacharacters() {
        let f = Fixture::new();
        let name = "alpha 'quoted' \"cost$\"; $(nothing)";
        f.file(name, 0o644);
        for prefix in ["cat alpha", "cat 'alpha", "cat \"alpha", "cat alpha\\ "] {
            let line = f.apply(prefix, "");
            let program = syntax::parse(&line).unwrap();
            let word = &program[0].1[0].words[1];
            assert!(
                matches!(&word[..],[syntax::Part::Text(text)] if text==name),
                "{line:?}"
            );
            assert_eq!(f.run(prefix, "").candidates, [name]);
        }
        assert!(f.run("cat $HOME/alpha", "").candidates.is_empty());
        assert!(f.run("cat `printf hello`", "").candidates.is_empty());
        assert!(!f.root.join("nothing").exists());
    }

    #[test]
    fn unicode_prefix_midword_completion_and_directory_suffix_are_stable() {
        let f = Fixture::new();
        f.file("海岸青", 0o600);
        f.file("海岸红", 0o600);
        f.file("éa", 0o600);
        f.file("êb", 0o600);
        assert_eq!(f.apply("cat 海", ""), "cat 海岸");
        assert_eq!(f.run("cat 海岸", "").candidates, ["海岸红", "海岸青"]);
        assert!(f.run("cat 海岸", "").replace.is_none());
        let line = "cat 海";
        assert!(complete(&f.state, line, 5).candidates.is_empty());
        assert!(
            complete(&f.state, line, line.len() + 1)
                .candidates
                .is_empty()
        );
        assert_eq!(f.apply("cat 海岸青", "old next"), "cat 海岸青 next");
        assert_eq!(f.apply("cat 海岸青", "old|pwd"), "cat 海岸青|pwd");
        fs::create_dir(f.root.join("my dir")).unwrap();
        f.file("my file", 0o644);
        assert_eq!(f.apply("cd my", ""), "cd my\\ dir/");
        f.file("my dir/target", 0o644);
        assert_eq!(f.apply("cat 'my dir/ta", ""), "cat 'my dir/target' ");
    }

    #[test]
    fn excludes_hidden_and_control_names_and_never_opens_special_files() {
        let f = Fixture::new();
        f.file(".hidden", 0o644);
        f.file("p\x1b[2J", 0o700);
        f.file("p\u{202e}spoof", 0o644);
        let fifo = CString::new(f.root.join("pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: fixture owns this new pathname and keeps the C string live for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(f.run("cat p", "").candidates.is_empty());
        assert!(
            !f.run("cat ", "")
                .candidates
                .iter()
                .any(|s| s.starts_with('.'))
        );
        assert_eq!(f.apply("cat .h", ""), "cat .hidden ");
    }
    #[test]
    fn git_cargo_and_zrun_complete_their_actual_command_positions() {
        let f = Fixture::new();
        f.file("status-file", 0o600);
        for line in [
            "git sw",
            "git -C other sw",
            "git -c color.ui=false sw",
            "true | /usr/bin/git sw",
            "command git sw",
            "async task -- git sw",
        ] {
            assert_eq!(f.run(line, "").candidates, ["switch"], "{line}");
            assert_eq!(
                f.apply(line, ""),
                format!("{}switch ", line.strip_suffix("sw").unwrap())
            );
        }
        let matches = f.run("git sta", "").candidates;
        assert_eq!(&matches[..2], ["stash", "status"]);
        assert!(!matches.contains(&"status-file".to_string()));
        assert!(f.run("git status --sho", "").replace.is_none()); // --short and --show-stash share --sho
        assert_eq!(f.apply("git status --shor", ""), "git status --short ");
        assert_eq!(f.apply("alia", ""), "alias ");
        assert_eq!(f.apply("unali", ""), "unalias ");
        assert_eq!(f.apply("ai", ""), "ai ");
        assert_eq!(f.apply("cargo bui", ""), "cargo build ");
        assert_eq!(f.apply("cargo build --rel", ""), "cargo build --release ");
        assert_eq!(f.apply("zrun ai com", ""), "zrun ai compact ");
        assert_eq!(
            f.apply("zrun ai connect --std", ""),
            "zrun ai connect --stdio "
        );
        assert_eq!(f.apply("git add -- sta", ""), "git add -- status-file ");
        assert_eq!(f.apply("git >sta", ""), "git >status-file ");
    }

    #[test]
    fn ranked_file_filter_and_menu_selection_keep_words_literal() {
        let f = Fixture::new();
        fs::create_dir(f.root.join("words")).unwrap();
        f.file("words/multi-a", 0o600);
        f.file("words/multi-b", 0o600);
        assert_eq!(f.run("cat words/mub", "").candidates, ["words/multi-b"]);
        assert_eq!(f.apply("git swt", ""), "git switch ");
        let name = "long name;$(no)";
        f.file(name, 0o600);
        let line = "cat 'lXYZ' tail";
        let (span, insert) = select(line, 6, name).unwrap();
        assert_eq!(span, 4..10);
        let mut line = line.to_owned();
        line.replace_range(span, &insert);
        let parsed = syntax::parse(&line).unwrap();
        assert!(matches!(&parsed[0].1[0].words[1][..],[syntax::Part::Text(s)] if s==name));
        assert!(matches!(&parsed[0].1[0].words[2][..],[syntax::Part::Text(s)] if s=="tail"));
        assert!(select("printf $X", 9, name).is_none());
        assert!(select("cat ", 4, "evil\x1b[2J").is_none());
    }

    #[test]
    fn git_completes_only_branch_refs_in_the_current_repository() {
        let mut f = Fixture::new();
        let admin = f.root.join(".git");
        fs::create_dir_all(admin.join("refs/heads/feature")).unwrap();
        fs::write(admin.join("refs/heads/main"), "").unwrap();
        fs::write(admin.join("refs/heads/feature/forms"), "").unwrap();
        fs::write(admin.join("refs/heads/main.lock"), "").unwrap();
        symlink(&f.root, admin.join("refs/heads/leak")).unwrap();
        fs::write(
            admin.join("packed-refs"),
            format!(
                "{} refs/heads/maint\n{} refs/remotes/origin/private\n",
                "a".repeat(40),
                "b".repeat(40)
            ),
        )
        .unwrap();
        let fifo = CString::new(admin.join("refs/heads/pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: the fixture owns the new pathname for this FIFO.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::create_dir(f.root.join("sub")).unwrap();
        f.state.cwd = f.root.join("sub");
        assert_eq!(f.run("git switch mai", "").candidates, ["main", "maint"]);
        assert_eq!(f.apply("git switch fea", ""), "git switch feature/forms ");
        assert_eq!(
            f.run("git switch ", "").candidates,
            ["feature/forms", "main", "maint"]
        );
        assert!(f.run("git -C /tmp switch mai", "").candidates.is_empty());
        assert!(
            f.run("GIT_DIR=/elsewhere git switch mai", "")
                .candidates
                .is_empty()
        );
        f.state.env.insert("GIT_DIR".into(), "/elsewhere".into());
        assert!(f.run("git switch mai", "").candidates.is_empty());
    }
}
