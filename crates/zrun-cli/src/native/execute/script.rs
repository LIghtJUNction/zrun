use super::{Control, Interrupt, State, Streams, error, execute, syntax};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
};

impl State {
    fn script_path(&self, name: &str, current_first: bool) -> io::Result<PathBuf> {
        if name.contains('/') {
            return Ok(self.cwd.join(name));
        }
        if current_first && self.cwd.join(name).is_file() {
            return Ok(self.cwd.join(name));
        }
        let path = self
            .env
            .get(OsStr::new("PATH"))
            .map_or(OsStr::new("/bin:/usr/bin"), |v| v.as_os_str());
        std::env::split_paths(path)
            .map(|dir| self.cwd.join(dir).join(name))
            .find(|p| p.is_file())
            .ok_or_else(|| error(format!("{name}: source file not found")))
    }

    fn evaluate_lines(
        &mut self,
        input: impl io::BufRead,
        streams: &Streams,
        control: &Arc<Control>,
        tty: bool,
        signal: Option<&Interrupt>,
    ) -> io::Result<u8> {
        let mut result = 0;
        super::super::stream_lines(input, |line| {
            for stmt in syntax::statements(line) {
                let program = syntax::parse(stmt)?;
                if program.is_empty() {
                    continue;
                }
                result = execute(self, &program, streams, control, tty, signal);
                if self.returning.is_some() || self.exit.is_some() {
                    return Ok(false);
                }
            }
            Ok(true)
        })?;
        Ok(result)
    }

    pub(super) fn evaluate(
        &mut self,
        argv: &[String],
        streams: &Streams,
        control: &Arc<Control>,
        tty: bool,
        signal: Option<&Interrupt>,
    ) -> io::Result<u8> {
        if self.eval_depth >= 64 {
            return Err(error("source/eval nesting limit reached"));
        }
        let (name, args) = argv.split_first().expect("evaluation command has a name");
        self.eval_depth += 1;
        let result = (|| {
            if name == "eval" {
                return self.evaluate_lines(
                    io::Cursor::new(args.join(" ")),
                    streams,
                    control,
                    tty,
                    signal,
                );
            }
            let (file, args) = args
                .split_first()
                .ok_or_else(|| error(format!("{name}: expected filename")))?;
            let path = self.script_path(file, name == "source")?;
            let input = File::open(Path::new(&path))?;
            if input.metadata()?.is_dir() {
                return Err(error(format!("{file}: is a directory")));
            }
            let saved = (!args.is_empty()).then(|| {
                let mut positions = vec![self.positional[0].clone()];
                positions.extend(args.iter().map(Into::into));
                std::mem::replace(&mut self.positional, positions)
            });
            self.source_depth += 1;
            let status = self.evaluate_lines(BufReader::new(input), streams, control, tty, signal);
            self.source_depth -= 1;
            if let Some(saved) = saved {
                self.positional = saved;
            }
            let returned = self.returning.take();
            status.map(|v| returned.unwrap_or(v))
        })();
        self.eval_depth -= 1;
        result
    }
}
