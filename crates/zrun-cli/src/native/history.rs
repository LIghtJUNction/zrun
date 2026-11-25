//! Bounded interactive history shared by the user's local zrun shells.
use std::collections::VecDeque;
use std::env;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_ENTRIES: usize = 256;
const MAX_LINE: usize = 4096;
const MAX_FILE: usize = MAX_ENTRIES * (MAX_LINE + 1);

pub(super) struct History {
    directory: Option<PathBuf>,
    entries: VecDeque<String>,
    // Commands entered locally since the last successful merge.
    pending: VecDeque<String>,
}

impl History {
    pub(super) fn load() -> Self {
        Self::at(state_dir(
            env::var_os("XDG_STATE_HOME"),
            env::var_os("HOME"),
        ))
    }

    fn at(directory: Option<PathBuf>) -> Self {
        let entries = directory
            .as_deref()
            .and_then(|dir| check_directory(dir).and_then(|()| read_history(dir)).ok())
            .unwrap_or_default();
        Self {
            directory,
            entries,
            pending: VecDeque::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn get(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(String::as_str)
    }

    pub(super) fn suggestion(&self, prefix: &str) -> Option<&str> {
        if prefix.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .rev()
            .find(|line| line.len() > prefix.len() && line.starts_with(prefix))
            .map(String::as_str)
    }

    pub(super) fn push(&mut self, line: &str) {
        if !recordable(line) {
            return;
        }
        remember(&mut self.entries, line);
        if let Some(directory) = &self.directory {
            remember(&mut self.pending, line);
            if let Ok(entries) = merge(directory, &self.pending) {
                self.entries = entries;
                self.pending.clear();
            }
        }
    }
}

fn state_dir(state: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |path: Option<OsString>| path.map(PathBuf::from).filter(|p| p.is_absolute());
    absolute(state)
        .or_else(|| absolute(home).map(|home| home.join(".local/state")))
        .map(|base| base.join("zrun"))
}

fn recordable(line: &str) -> bool {
    !line.trim().is_empty()
        && !line.starts_with(' ')
        && line.len() <= MAX_LINE
        && !line.chars().any(char::is_control)
}

fn remember(entries: &mut VecDeque<String>, line: &str) {
    if entries.back().is_some_and(|last| last == line) {
        return;
    }
    if entries.len() == MAX_ENTRIES {
        entries.pop_front();
    }
    entries.push_back(line.to_owned());
}

fn check_directory(dir: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    // SAFETY: geteuid takes no arguments and accesses no caller memory.
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn private_file(path: &Path, write: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create(write)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: geteuid takes no arguments and accesses no caller memory.
    // A reader can open the previous snapshot just before rename unlinks it.
    if !meta.is_file()
        || meta.nlink() > 1
        || (write && meta.nlink() == 0)
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

fn read_history(dir: &Path) -> io::Result<VecDeque<String>> {
    let file = match private_file(&dir.join("history"), false) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        result => result?,
    };
    let mut text = String::new();
    file.take(MAX_FILE as u64 + 1).read_to_string(&mut text)?;
    if text.len() > MAX_FILE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut entries = VecDeque::new();
    for line in text.lines().filter(|line| recordable(line)) {
        remember(&mut entries, line);
    }
    Ok(entries)
}

fn merge(dir: &Path, pending: &VecDeque<String>) -> io::Result<VecDeque<String>> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    check_directory(dir)?;
    // The lock has a stable inode across atomic replacements of the data file.
    let lock = private_file(&dir.join("history.lock"), true)?;
    lock.lock()?;
    let mut entries = read_history(dir)?;
    for line in pending {
        remember(&mut entries, line);
    }
    let temp = dir.join("history.tmp");
    let mut file = private_file(&temp, true)?;
    file.set_len(0)?;
    let mut contents = String::new();
    for line in &entries {
        contents.push_str(line);
        contents.push('\n');
    }
    file.write_all(contents.as_bytes())?;
    file.sync_data()?;
    fs::rename(temp, dir.join("history"))?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = env::temp_dir().join(format!(
                "zrun-history-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&dir).unwrap();
            Self(dir)
        }
        fn history(&self) -> History {
            History::at(Some(self.0.join("zrun")))
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn xdg_state_and_home_fallback_require_absolute_paths() {
        let home = Some(OsString::from("/example/home"));
        assert_eq!(
            state_dir(Some("/example/state".into()), home.clone()),
            Some("/example/state/zrun".into())
        );
        for state in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                state_dir(state, home.clone()),
                Some("/example/home/.local/state/zrun".into())
            );
        }
        assert_eq!(state_dir(None, Some("relative".into())), None);
    }

    #[test]
    fn suggestions_preserve_text_and_prefer_the_recent_strict_prefix_match() {
        let boxdir = Sandbox::new();
        let mut history = boxdir.history();
        for cmd in [
            "echo 一",
            "echo two",
            "echo",
            "echo",
            " echo private",
            "",
            "   ",
            "x\ny",
            "x\x1b[2J",
        ] {
            history.push(cmd);
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0), Some("echo 一"));
        assert_eq!(history.get(3), None);
        assert_eq!(history.suggestion("ech"), Some("echo"));
        assert_eq!(history.suggestion("echo"), Some("echo two"));
        assert_eq!(history.suggestion("echo 一"), None);
        assert_eq!(history.suggestion(""), None);
        let dir = boxdir.0.join("zrun");
        let stored = fs::read_to_string(dir.join("history")).unwrap();
        assert_eq!(stored, "echo 一\necho two\necho\n");
        for path in ["history", "history.lock"] {
            assert_eq!(fs::metadata(dir.join(path)).unwrap().mode() & 0o777, 0o600);
        }
        assert_eq!(fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(boxdir.history().get(1), Some("echo two"));
    }

    #[test]
    fn concurrent_sessions_merge_instead_of_replacing_stale_snapshots() {
        let boxdir = Sandbox::new();
        let barrier = Arc::new(Barrier::new(4));
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let mut shell = boxdir.history();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for i in 0..12 {
                        shell.push(&format!("job {worker} {i}"));
                    }
                });
            }
        });
        let history = boxdir.history();
        assert_eq!(history.len(), 48);
        for worker in 0..4 {
            let prefix = format!("job {worker} ");
            let actual: Vec<_> = history
                .entries
                .iter()
                .filter_map(|cmd| cmd.strip_prefix(&prefix))
                .collect();
            assert_eq!(actual, (0..12).map(|i| i.to_string()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn stale_temporary_file_and_transient_failure_preserve_committed_history() {
        let boxdir = Sandbox::new();
        let dir = boxdir.0.join("zrun");
        let mut shell = boxdir.history();
        shell.push("first");
        fs::create_dir(dir.join("history.tmp")).unwrap();
        shell.push("second");
        assert_eq!(shell.get(1), Some("second"));
        assert_eq!(boxdir.history().len(), 1);
        fs::remove_dir(dir.join("history.tmp")).unwrap();
        private_file(&dir.join("history.tmp"), true)
            .unwrap()
            .write_all(b"incomplete")
            .unwrap();
        let mut other = boxdir.history();
        other.push("another session");
        shell.push("third");
        assert_eq!(
            fs::read_to_string(dir.join("history")).unwrap(),
            "first\nanother session\nsecond\nthird\n"
        );
    }

    #[test]
    fn unsafe_or_invalid_store_falls_back_to_memory() {
        let boxdir = Sandbox::new();
        let dir = boxdir.0.join("zrun");
        let private = boxdir.0.join("unrelated");
        DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::write(&private, "unrelated contents\n").unwrap();
        symlink(&private, dir.join("history")).unwrap();
        let mut shell = boxdir.history();
        shell.push("local");
        assert_eq!(shell.get(0), Some("local"));
        assert_eq!(fs::read_to_string(private).unwrap(), "unrelated contents\n");
        fs::remove_file(dir.join("history")).unwrap();
        fs::write(dir.join("history"), [0xff]).unwrap();
        fs::set_permissions(dir.join("history"), fs::Permissions::from_mode(0o600)).unwrap();
        shell.push("next");
        assert_eq!(shell.get(1), Some("next"));
        assert_eq!(fs::read(dir.join("history")).unwrap(), [0xff]);
    }

    #[test]
    fn caps_file_and_session_history_to_recent_complete_entries() {
        let boxdir = Sandbox::new();
        let dir = boxdir.0.join("zrun");
        let mut shell = boxdir.history();
        shell.push(&"x".repeat(MAX_LINE + 1));
        assert_eq!(shell.len(), 0);
        assert!(!dir.exists());
        for i in 0..MAX_ENTRIES + 7 {
            shell.push(&format!("cmd {i}"));
        }
        assert_eq!(shell.len(), MAX_ENTRIES);
        assert_eq!(shell.get(0), Some("cmd 7"));
        assert_eq!(boxdir.history().get(0), Some("cmd 7"));
        assert_eq!(
            fs::read_to_string(dir.join("history"))
                .unwrap()
                .lines()
                .count(),
            MAX_ENTRIES
        );
    }
}
