//! Read only local branch names. Never run Git, inspect working-tree files or load config.
use super::{State, display_safe, match_quality};
use std::{
    collections::BTreeSet, ffi::OsStr, fs, io::Read, os::unix::fs::OpenOptionsExt, path::Path,
};

const MAX_ENTRIES: usize = 512;
const MAX_PACKED: usize = 256 * 1024;

fn usable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with('.'))
        && display_safe(name)
        && !name.starts_with('-')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name
            .chars()
            .any(|c| c.is_whitespace() || "~^:?*[\\".contains(c))
}

fn dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

pub(super) fn local(state: &State, prefix: &str, matches: &mut BTreeSet<String>) {
    if ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE"]
        .iter()
        .any(|key| state.env.contains_key(OsStr::new(key)))
    {
        return;
    }
    for ancestor in state.cwd.ancestors().take(32) {
        let marker = ancestor.join(".git");
        let Ok(meta) = fs::symlink_metadata(&marker) else {
            continue;
        };
        // Linked worktrees can point to a different private tree. Leave external metadata alone.
        if !meta.is_dir() {
            return;
        }
        let refs = marker.join("refs");
        let heads = refs.join("heads");
        if dir(&refs) && dir(&heads) {
            let mut queue = vec![(heads, String::new())];
            let mut remaining = MAX_ENTRIES;
            while let Some((directory, parent)) = queue.pop() {
                let Ok(entries) = fs::read_dir(directory) else {
                    continue;
                };
                for entry in entries.take(remaining).flatten() {
                    remaining -= 1;
                    let Ok(part) = entry.file_name().into_string() else {
                        continue;
                    };
                    if part.starts_with('.') || !usable(&part) {
                        continue;
                    }
                    let name = format!("{parent}{part}");
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() && name.len() < 256 {
                        queue.push((entry.path(), format!("{name}/")));
                    } else if kind.is_file() && match_quality(prefix, &name).is_some() {
                        matches.insert(name);
                    }
                }
                if remaining == 0 {
                    break;
                }
            }
        }
        let packed = marker.join("packed-refs");
        if !fs::symlink_metadata(&packed).is_ok_and(|m| m.is_file() && m.len() <= MAX_PACKED as u64)
        {
            return;
        }
        let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(packed)
        else {
            return;
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            return;
        }
        let mut text = String::new();
        if file
            .take(MAX_PACKED as u64 + 1)
            .read_to_string(&mut text)
            .is_err()
            || text.len() > MAX_PACKED
        {
            return;
        }
        for line in text.lines() {
            let Some((hash, name)) = line.split_once(" refs/heads/") else {
                continue;
            };
            if matches!(hash.len(), 40 | 64)
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && usable(name)
                && match_quality(prefix, name).is_some()
            {
                matches.insert(name.to_owned());
            }
        }
        return;
    }
}
