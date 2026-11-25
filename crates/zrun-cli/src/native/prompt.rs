//! The interactive prompt only inspects Git's marker and HEAD.
use super::State;
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
};

pub(super) fn current(state: &State) -> String {
    if !io::stderr().is_terminal() {
        return String::new();
    }
    let var = |name: &str| state.env.get(OsStr::new(name)).map(|v| v.as_os_str());
    if matches!(
        var("TERM").and_then(OsStr::to_str),
        None | Some("" | "dumb")
    ) || var("ZRUN_PROMPT") == Some(OsStr::new("plain"))
    {
        return "zrun> ".into();
    }
    let mut size = libc::winsize::default();
    // SAFETY: TIOCGWINSZ writes one winsize into the provided live, aligned value.
    let columns = match unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } {
        0 if size.ws_col > 0 => usize::from(size.ws_col),
        _ => 80,
    };
    let color = var("NO_COLOR").is_none_or(OsStr::is_empty);
    let home = var("HOME").map(Path::new).filter(|p| p.is_absolute());
    let path = match home.and_then(|p| state.cwd.strip_prefix(p).ok()) {
        Some(relative) if relative.as_os_str().is_empty() => "~".into(),
        Some(relative) => Path::new("~").join(relative),
        None => state.cwd.clone(),
    };
    let branch = (columns >= 40).then(|| git_branch(&state.cwd)).flatten();
    render(&path, branch.as_deref(), state.status, columns, color)
}

// Only regular metadata files are read. A FIFO or symlink named HEAD must never
// block the command line or substitute another file's contents.
fn small_file(path: &Path) -> Option<Vec<u8>> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= 4096).then_some(bytes)
}

fn git_branch(cwd: &Path) -> Option<String> {
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(".git");
        let kind = match fs::symlink_metadata(&marker) {
            Ok(meta) => meta.file_type(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let dir = if kind.is_dir() {
            marker
        } else if kind.is_file() {
            let bytes = small_file(&marker)?;
            let location = bytes.trim_ascii_end().strip_prefix(b"gitdir: ")?;
            if location.is_empty() {
                return None;
            }
            ancestor.join(OsStr::from_bytes(location))
        } else {
            return None;
        };
        let head = small_file(&dir.join("HEAD"))?;
        let branch = head.strip_suffix(b"\n").unwrap_or(&head);
        let branch = branch.strip_suffix(b"\r").unwrap_or(branch);
        let name = branch.strip_prefix(b"ref: refs/heads/")?;
        return (!name.is_empty()).then(|| String::from_utf8_lossy(name).into_owned());
    }
    None
}

fn label(raw: &str, budget: usize) -> String {
    let text: String = raw
        .chars()
        .map(|c| match c {
            c if c.is_control() => '?',
            '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}' => '?',
            c => c,
        })
        .collect();
    // UTF-8 byte length conservatively bounds display columns, including wide
    // characters. Keep an intact suffix so long paths retain their leaf name.
    if text.len() <= budget {
        return text;
    }
    if budget < 4 {
        return String::new();
    }
    let start = text.ceil_char_boundary(text.len() - (budget - 3));
    format!("…{}", &text[start..])
}

fn render(path: &Path, branch: Option<&str>, status: u8, columns: usize, color: bool) -> String {
    if columns < 3 {
        return ">".into();
    }
    let (blue, dim, red, reset, arrow) = if color {
        ("\x1b[1;34m", "\x1b[2m", "\x1b[31m", "\x1b[0m", "❯")
    } else {
        ("", "", "", "", ">")
    };
    let mut available = columns.saturating_sub(8).min(88);
    let mut output = String::new();
    let failed = if status != 0 {
        format!("!{status} ")
    } else {
        String::new()
    };
    available = available.saturating_sub(failed.len() + 2);
    let branch = branch.map(|s| label(s, available / 3)).unwrap_or_default();
    if !branch.is_empty() {
        available = available.saturating_sub(branch.len() + 3);
    }
    let path = label(&path.to_string_lossy(), available.saturating_sub(1));
    if !path.is_empty() {
        output.push_str(&format!("{blue}{path}{reset} "));
    }
    if !branch.is_empty() {
        output.push_str(&format!("{dim}({branch}){reset} "));
    }
    if !failed.is_empty() && columns >= failed.len() + 2 {
        output.push_str(&format!("{red}{failed}{reset}"));
    }
    output.push_str(&format!("{dim}{arrow}{reset} "));
    output
}
