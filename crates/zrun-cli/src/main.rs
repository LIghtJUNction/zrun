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
mod ai;
#[cfg(target_os = "linux")]
mod bytes;
#[cfg(target_os = "linux")]
mod execution;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(target_os = "linux")]
mod mcp;
#[cfg(target_os = "linux")]
mod native;
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
#[cfg(target_os = "linux")]
mod valkey;

const HELP: &str = "Usage:
  zrun                            Enter the native Linux terminal shell
  zrun --shell [ARGS...]          Enter the configured compatibility shell
  zrun COMMAND [ARGS...]          Execute a program with literal arguments
  zrun FILE [ARGS...]             Run a native script or its declared interpreter
  zrun -c SCRIPT [ARGS...]        Evaluate a script with inherited terminal I/O
  zrun run SCRIPT                 Run a managed command with captured output
  zrun batch [--ready]            Run JSONL commands, optionally emit ready remote batches
  zrun ai [config|connect|run|compact]  Connect an AI client or summarize output
  zrun mcp [--stdio | --http ADDRESS [--unix [PATH]] | --unix [PATH]]
  zrun --zmem CLASS [COMMAND...]  Run zrun and its child commands in one zmem scope
  zrun --help
  zrun --version
  zrun --native-shell-version     Print the native Linux shell interface version

ZRUN_SHELL overrides the Unix zrun/shell configuration file and SHELL.
ZRUN_WORKERS selects the worker count for the managed runtime.
ZRUN_IO_DRIVER selects thread (default) or worker at multiple workers.
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
    let mut args = argv.peekable();
    let first = args.next();
    #[cfg(target_os = "linux")]
    let first = {
        let mut operand = first;
        while matches!(
            operand.as_deref().and_then(OsStr::to_str),
            Some("-l" | "--login" | "-i" | "-li" | "-il")
        ) && args.peek().is_some()
        {
            operand = args.next();
        }
        if matches!(
            operand.as_deref().and_then(OsStr::to_str),
            Some("-lc" | "-ic" | "-lic" | "-ilc")
        ) {
            Some("-c".into())
        } else {
            operand
        }
    };
    #[cfg(target_os = "linux")]
    let first = {
        if first.as_deref() == Some(OsStr::new("--")) {
            let operand = args.peek().map(|arg| std::path::Path::new(".").join(arg));
            if let Some(path) = operand.filter(|p| is_source_file(p.as_os_str())) {
                args.next();
                Some(path.into_os_string())
            } else {
                first
            }
        } else if let Some(path) = first
            .as_deref()
            .filter(|arg| arg.as_encoded_bytes().starts_with(b"-") && is_script_operand(arg))
        {
            Some(std::path::Path::new(".").join(path).into_os_string())
        } else {
            first
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(path) = first.as_deref().filter(|arg| is_source_file(arg)) {
        let path = std::path::Path::new(path);
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
        if let Some(mut command) = shebang_command(&prefix)? {
            command.arg(path).args(args);
            return enter_shell(command);
        }
        file.rewind()?;
        let positional = std::iter::once(path.as_os_str().to_owned())
            .chain(args)
            .collect();
        return Ok(native::source(io::BufReader::new(file), positional)?);
    }
    #[cfg(target_os = "linux")]
    if io::stdin().is_terminal()
        && (first.is_none()
            || (matches!(
                first.as_deref().and_then(OsStr::to_str),
                Some("-i" | "-l" | "--login" | "-li" | "-il")
            ) && args.peek().is_none()))
    {
        return Ok(native::interactive()?);
    }
    #[cfg(target_os = "linux")]
    if first.is_none()
        || matches!(
            first.as_deref().and_then(OsStr::to_str),
            Some("-i" | "-l" | "--login" | "-li" | "-il" | "-s" | "-")
        )
    {
        args.next_if(|arg| arg == "--");
        let positional = std::iter::once(OsString::from("zrun"))
            .chain(args)
            .collect();
        return Ok(native::source(io::stdin().lock(), positional)?);
    }
    if first.as_deref() == Some(OsStr::new("--shell")) {
        let mut command = Command::new(shell()?);
        #[cfg(unix)]
        if invoked
            .as_ref()
            .and_then(|arg| std::path::Path::new(arg).file_name())
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"-"))
        {
            use std::os::unix::process::CommandExt;
            let backend = shell()?;
            let mut login = OsString::from("-");
            login.push(backend.file_name().unwrap_or(backend.as_os_str()));
            command.arg0(login).arg("-l");
        }
        command.args(args);
        return enter_shell(command);
    }
    #[cfg(target_os = "linux")]
    if first.as_deref() == Some(OsStr::new("-c")) {
        let script = args.next().ok_or("-c needs a command")?;
        return Ok(native::command(&script, args.collect())?);
    }
    match first.as_deref().and_then(OsStr::to_str) {
        Some("--help" | "-h") => {
            println!("{HELP}");
            Ok(0)
        }
        #[cfg(target_os = "linux")]
        Some("--native-shell-version") if args.peek().is_none() => {
            println!("1");
            Ok(0)
        }
        #[cfg(not(target_os = "linux"))]
        Some("--native-shell-version") => Err("the native zrun shell requires Linux".into()),
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
        #[cfg(target_os = "linux")]
        Some("ai") => ai::run(args),
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
                false,
            )
        }
        Some("batch") => {
            let ready_order = args.next_if(|arg| arg == "--ready").is_some();
            if args.next().is_some() {
                return Err("zrun batch takes commands from stdin".into());
            }
            managed(shell()?, None, ready_order)
        }
        #[cfg(target_os = "linux")]
        _ if first
            .as_deref()
            .is_some_and(|arg| !arg.as_encoded_bytes().starts_with(b"-")) =>
        {
            use std::os::unix::process::CommandExt;
            let name = first.expect("program operand");
            let error = Command::new(&name).args(args).exec();
            eprintln!("zrun: {}: {error}", name.to_string_lossy());
            Ok(if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
        #[cfg(target_os = "linux")]
        _ => Err("unsupported zrun option; see zrun --help".into()),
        #[cfg(not(target_os = "linux"))]
        _ => {
            let interactive = first.is_none() && io::stdin().is_terminal();
            let backend = shell()?;
            let mut command = Command::new(&backend);
            #[cfg(unix)]
            if invoked
                .as_ref()
                .and_then(|arg| std::path::Path::new(arg).file_name())
                .is_some_and(|name| name.as_encoded_bytes().starts_with(b"-"))
            {
                use std::os::unix::process::CommandExt;
                let mut login_name = OsString::from("-");
                login_name.push(backend.file_name().unwrap_or(backend.as_os_str()));
                command.arg0(login_name);
                // Shebang wrappers lose the caller's argv[0] when the kernel opens the interpreter.
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

#[cfg(target_os = "linux")]
fn is_source_file(arg: &OsStr) -> bool {
    if arg.as_encoded_bytes().starts_with(b"-") {
        return false;
    }
    if is_script_operand(arg) {
        return true;
    }
    if matches!(arg.to_str(), Some("run" | "batch" | "mcp" | "ai")) {
        return false;
    }
    let path = std::path::Path::new(arg);
    if !path.is_file() {
        return false;
    }
    // A regular executable such as /bin/ls is a program operand. A text
    // script (including executable text with no shebang) is native input.
    let mut magic = [0; 4];
    !std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|()| magic == *b"\x7fELF")
}

#[cfg(target_os = "linux")]
fn is_script_operand(arg: &OsStr) -> bool {
    std::path::Path::new(arg).extension() == Some(OsStr::new("zrun"))
}

#[cfg(target_os = "linux")]
fn shebang_command(prefix: &[u8]) -> io::Result<Option<Command>> {
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    let Some(line) = prefix.strip_prefix(b"#!") else {
        return Ok(None);
    };
    let line = line.split(|byte| *byte == b'\n').next().unwrap_or_default();
    if line.len() > 253 || line.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid script shebang",
        ));
    }
    let mut words = line
        .trim_ascii()
        .splitn(2, |byte| matches!(byte, b' ' | b'\t'));
    let interpreter = Path::new(OsStr::from_bytes(words.next().unwrap_or_default()));
    let argument = words.next().map(<[u8]>::trim_ascii_start);
    if !interpreter.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "script interpreter must be absolute",
        ));
    }
    if selects_self(interpreter) {
        return Ok(None);
    }
    if interpreter.file_name() == Some(OsStr::new("env")) {
        let Some(argument) = argument else {
            return Ok(None);
        };
        // GNU env may interpret quoting, assignments and options before
        // selecting its executable. Handle a plain command name and leave
        // ambiguous forms to the native reader so they cannot relaunch us.
        let target = if let Some(command) = argument.strip_prefix(b"-S ") {
            command
                .trim_ascii_start()
                .split(|byte| matches!(byte, b' ' | b'\t'))
                .next()
                .unwrap_or_default()
        } else {
            argument
        };
        if target.is_empty()
            || target.starts_with(b"-")
            || !target.iter().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.' | b'+')
            })
        {
            return Ok(None);
        }
        let target = Path::new(OsStr::from_bytes(target));
        if matches!(
            target.file_name().and_then(OsStr::to_str),
            Some("env" | "zrun")
        ) || selects_self(target)
        {
            return Ok(None);
        }
    }
    let mut command = Command::new(interpreter);
    if let Some(argument) = argument {
        command.arg(OsStr::from_bytes(argument));
    }
    Ok(Some(command))
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
fn read_batch(
    input: impl io::BufRead,
) -> Result<Vec<execution::Request>, Box<dyn std::error::Error>> {
    use std::io::BufRead;

    let mut input = input.take(execution::Batch::MAX_INPUT as u64 + 1);
    let mut line = String::new();
    let mut requests: Result<_, serde_json::Error> = Ok(Vec::new());
    while input.read_line(&mut line)? != 0 {
        if input.limit() == 0 {
            return Err("batch input exceeds the byte limit".into());
        }
        if !line.trim().is_empty() {
            // Retain the first parse error, but consume stdin so producers
            // can send their entire batch and the byte limit still applies.
            requests = requests.and_then(|mut requests| {
                requests.push(serde_json::from_str::<execution::Request>(&line)?);
                Ok(requests)
            });
        }
        line.clear();
    }
    Ok(requests?)
}

#[cfg(target_os = "linux")]
fn managed(
    shell: PathBuf,
    script: Option<String>,
    ready_order: bool,
) -> Result<u8, Box<dyn std::error::Error>> {
    use execution::Request;

    let requests = if let Some(script) = script {
        vec![Request::new(script)]
    } else {
        let requests = read_batch(io::stdin().lock())?;
        let mut output = io::stdout().lock();
        #[derive(serde::Serialize)]
        struct Record {
            index: usize,
            #[serde(flatten)]
            delivery: execution::Delivery,
        }
        execute_batch(shell, requests, ready_order, |index, delivery| {
            serde_json::to_writer(&mut output, &Record { index, delivery })?;
            writeln!(output)
        })?;
        output.flush()?;
        return Ok(0);
    };
    let mut status = 1;
    execute_batch(shell, requests, false, |_, delivery| {
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
fn managed(
    _shell: PathBuf,
    _script: Option<String>,
    _ready_order: bool,
) -> Result<u8, Box<dyn std::error::Error>> {
    Err("managed command execution currently requires Linux".into())
}

#[cfg(target_os = "linux")]
fn execute_batch(
    shell: PathBuf,
    requests: Vec<execution::Request>,
    ready_order: bool,
    record: impl FnMut(usize, execution::Delivery) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(path) = env::var_os("ZRUN_SOCKET") {
        mcp::run_remote(PathBuf::from(path), requests, ready_order, record)
    } else {
        execution::Batch::new(shell)?.execute(requests, record)
    }
}
