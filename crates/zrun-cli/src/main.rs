use std::env;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

#[cfg(target_os = "linux")]
mod bytes;
#[cfg(target_os = "linux")]
mod execution;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(target_os = "linux")]
mod mcp;
#[cfg(target_os = "linux")]
mod output;
#[cfg(target_os = "linux")]
mod plugin;
#[cfg(target_os = "linux")]
mod policy;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod script;

const HELP: &str = "Usage:
  zrun [shell arguments...]       Enter the configured terminal shell
  zrun FILE.zrun [ARGS...]        Run a shell file or a # zrun:1 async program
  zrun -c SCRIPT [ARGS...]        Evaluate a script with inherited terminal I/O
  zrun run SCRIPT                 Run a managed command with captured output
  zrun batch                      Run a batch of command objects from JSONL stdin
  zrun mcp [--stdio | --http ADDRESS [--unix [PATH]] | --unix [PATH]]
  zrun --zmem CLASS [COMMAND...]  Run zrun and its child commands in one zmem scope
  zrun --help
  zrun --version

ZRUN_SHELL overrides the Unix zrun/shell configuration file and SHELL.
ZRUN_WORKERS selects the worker count for the managed runtime.
--unix without PATH uses $XDG_RUNTIME_DIR/zrun/mcp.sock.";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("zrun: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let mut argv = env::args_os();
    let invoked = argv.next();
    #[cfg(not(unix))]
    drop(invoked);
    let mut args = argv;
    let first = args.next();
    #[cfg(target_os = "linux")]
    if let Some(path) = first.as_ref().map(std::path::Path::new)
        && path.extension().is_some_and(|ext| ext == "zrun")
    {
        use std::io::Seek;
        let mut file = std::fs::File::open(path)?;
        let mut prefix = Vec::new();
        // Read the Linux shebang window plus enough bytes for the native header.
        (&mut file).take(256 + 10).read_to_end(&mut prefix)?;
        if script::has_native_header(&prefix) {
            let positional = args
                .map(|arg| {
                    arg.into_string()
                        .map_err(|_| "native .zrun arguments must be UTF-8")
                })
                .collect::<Result<Vec<_>, _>>()?;
            file.rewind()?;
            let mut source = String::new();
            file.take(execution::Batch::MAX_INPUT as u64 + 1)
                .read_to_string(&mut source)?;
            if source.len() > execution::Batch::MAX_INPUT {
                return Err("native .zrun program exceeds the byte limit".into());
            }
            return Ok(script::run(shell()?, &source, positional)?);
        }
    }
    match first.as_deref().and_then(OsStr::to_str) {
        Some("--help" | "-h") => {
            println!("{HELP}");
            Ok(0)
        }
        Some("--version") => {
            println!("zrun {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Some("--zmem") => {
            #[cfg(target_os = "linux")]
            {
                let class = args.next().ok_or("--zmem requires a workload class")?;
                if !matches!(
                    class.to_str(),
                    Some("background" | "interactive" | "disposable")
                ) {
                    return Err("zmem class must be background, interactive, or disposable".into());
                }
                let mut command = Command::new("zmem");
                command
                    .args(["run", "--class"])
                    .arg(class)
                    .arg("--")
                    .arg(env::current_exe()?)
                    .args(args);
                enter_shell(command)
            }
            #[cfg(not(target_os = "linux"))]
            Err("zmem integration requires Linux".into())
        }
        Some("mcp") => {
            #[cfg(target_os = "linux")]
            mcp::serve(shell()?, args)?;
            #[cfg(not(target_os = "linux"))]
            return Err("the MCP server currently requires Linux".into());
            #[cfg(target_os = "linux")]
            Ok(0)
        }
        Some("run") => {
            let script = args.next().ok_or("zrun run requires a script")?;
            if args.next().is_some() {
                return Err("pass the full script as one argument".into());
            }
            managed(
                shell()?,
                Some(script.into_string().map_err(|_| "script is not UTF-8")?),
            )
        }
        Some("batch") => {
            if args.next().is_some() {
                return Err("zrun batch takes commands from stdin".into());
            }
            managed(shell()?, None)
        }
        _ => {
            let interactive = first.is_none() && io::stdin().is_terminal();
            let mut command = Command::new(shell()?);
            #[cfg(unix)]
            if invoked
                .as_ref()
                .and_then(|arg| std::path::Path::new(arg).file_name())
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"-"))
            {
                command.arg("-l");
            }
            command.args(first.into_iter().chain(args));
            if interactive {
                #[cfg(unix)]
                command.arg("-i");
            }
            enter_shell(command)
        }
    }
}

fn shell() -> io::Result<PathBuf> {
    let explicit = env::var_os("ZRUN_SHELL").filter(|value| !value.is_empty());
    #[cfg(unix)]
    let fallback = || {
        env::var_os("SHELL")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("/bin/sh"))
    };
    #[cfg(windows)]
    let fallback = || {
        env::var_os("COMSPEC")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("cmd.exe"))
    };
    let explicit = explicit.map(PathBuf::from);
    #[cfg(unix)]
    let explicit = match explicit {
        Some(path) => Some(path),
        None => configured_shell()?,
    };
    let selected = explicit.unwrap_or_else(|| PathBuf::from(fallback()));
    #[cfg(unix)]
    let recursive = selects_self(&selected);
    #[cfg(not(unix))]
    let recursive = selected == std::path::Path::new("zrun")
        || env::current_exe().ok().is_some_and(|this| {
            selected
                .canonicalize()
                .is_ok_and(|candidate| candidate == this)
        });
    if recursive {
        #[cfg(unix)]
        return Ok("/bin/sh".into());
        #[cfg(windows)]
        return Ok("cmd.exe".into());
    }
    Ok(selected)
}

#[cfg(unix)]
fn configured_shell() -> io::Result<Option<PathBuf>> {
    let directory = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        });
    let Some(path) = directory.map(|directory| directory.join("zrun/shell")) else {
        return Ok(None);
    };
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain one absolute shell path", path.display()),
        )
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    const MAX_CONFIG_BYTES: u64 = 4096;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(invalid());
    }
    let mut text = String::new();
    std::fs::File::open(&path)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)?;
    let value = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    let shell = PathBuf::from(value);
    if text.len() as u64 > MAX_CONFIG_BYTES
        || !shell.is_absolute()
        || value.contains(['\0', '\r', '\n'])
    {
        return Err(invalid());
    }
    Ok(Some(shell))
}

#[cfg(unix)]
fn selects_self(selected: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(this) = env::current_exe().and_then(std::fs::metadata) else {
        return false;
    };
    let candidate = if selected.as_os_str().as_encoded_bytes().contains(&b'/') {
        std::fs::metadata(selected).ok()
    } else {
        let path = env::var_os("PATH").unwrap_or_else(|| OsString::from("/bin:/usr/bin"));
        env::split_paths(&path).find_map(|directory| {
            let metadata = std::fs::metadata(directory.join(selected)).ok()?;
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(metadata)
        })
    };
    candidate
        .is_some_and(|candidate| candidate.dev() == this.dev() && candidate.ino() == this.ino())
}

#[cfg(unix)]
fn enter_shell(mut command: Command) -> Result<u8, Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;
    Err(command.exec().into())
}

#[cfg(not(unix))]
fn enter_shell(mut command: Command) -> Result<u8, Box<dyn std::error::Error>> {
    Ok(exit_code(command.status()?.code().unwrap_or(1)))
}

#[cfg(any(target_os = "linux", not(unix)))]
fn exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

#[cfg(target_os = "linux")]
fn managed(shell: PathBuf, script: Option<String>) -> Result<u8, Box<dyn std::error::Error>> {
    use execution::{Batch, Request};

    let requests = if let Some(script) = script {
        vec![Request::new(script)]
    } else {
        let mut input = String::new();
        io::stdin()
            .lock()
            .take(Batch::MAX_INPUT as u64 + 1)
            .read_to_string(&mut input)?;
        if input.len() > Batch::MAX_INPUT {
            return Err("batch input exceeds the byte limit".into());
        }
        let requests: Result<Vec<_>, _> = input
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect();
        let mut output = io::BufWriter::new(io::stdout().lock());
        #[derive(serde::Serialize)]
        struct Record {
            index: usize,
            #[serde(flatten)]
            delivery: execution::Delivery,
        }
        execute_batch(shell, requests?, |index, delivery| {
            serde_json::to_writer(&mut output, &Record { index, delivery })?;
            writeln!(output)
        })?;
        output.flush()?;
        return Ok(0);
    };
    let mut status = 1;
    execute_batch(shell, requests, |_, delivery| {
        let result = delivery.result;
        io::stdout().lock().write_all(&result.stdout)?;
        io::stderr().lock().write_all(&result.stderr)?;
        if result.truncated {
            eprintln!("zrun: captured output exceeded its limit");
        }
        if let Some(error) = &result.error {
            eprintln!("zrun: {error}");
        }
        status = exit_code(result.exit_code);
        Ok(())
    })?;
    Ok(status)
}

#[cfg(not(target_os = "linux"))]
fn managed(_shell: PathBuf, _script: Option<String>) -> Result<u8, Box<dyn std::error::Error>> {
    Err("managed command execution currently requires Linux".into())
}

#[cfg(target_os = "linux")]
fn execute_batch(
    shell: PathBuf,
    requests: Vec<execution::Request>,
    record: impl FnMut(usize, execution::Delivery) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(path) = env::var_os("ZRUN_SOCKET") {
        mcp::run_remote(PathBuf::from(path), requests, record)
    } else {
        execution::Batch::new(shell)?.execute(requests, record)
    }
}
