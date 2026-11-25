//! Interactive configuration is local to the account. Batch and AI calls skip it.
use super::{Session, error};
use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(crate) fn path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".config"))
        })
        .map(|p| p.join("zrun/zrunrc"))
}

impl Session {
    pub(super) fn startup(&mut self) {
        if env::var_os("ZRUN_NO_RC").as_deref() == Some(OsStr::new("1")) {
            return;
        }
        if let Some(path) = path()
            && let Err(err) = self.read_rc(&path, true)
        {
            eprintln!("zrun: {}: {err}", path.display());
        }
    }
    fn read_rc(&mut self, path: &Path, optional: bool) -> io::Result<()> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if optional && e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(error("expected a regular configuration file"));
        }
        const LIMIT: usize = 1024 * 1024;
        let mut source = String::new();
        file.take(LIMIT as u64 + 1).read_to_string(&mut source)?;
        if source.len() > LIMIT {
            return Err(error("configuration exceeds 1 MiB"));
        }
        for (number, line) in source.lines().enumerate() {
            self.line(line, false, None)
                .map_err(|e| error(format!("line {}: {e}", number + 1)))?;
            if self.state.exit.is_some() {
                break;
            }
        }
        Ok(())
    }
}
