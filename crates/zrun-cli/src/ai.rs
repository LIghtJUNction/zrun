//! Local AI client entry points share the native executor and its admission policy.
use crate::{execution::Request, mcp::compact::project};
use serde_json::{Value, json};
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

const HELP: &str = "Usage:
  zrun ai config                 Print an MCP client configuration
  zrun ai connect [TRANSPORT]    Serve MCP on stdio, a private Unix socket, or authenticated loopback
  zrun ai run [--save DIR] SCRIPT [ARGS...]
                                 Run once, summarize stdout/stderr and preserve the exit code
  zrun ai compact [--save FILE]  Summarize up to 16 MiB of standard input

Use --save with a new path to retain exact captured bytes. A connected client can use
shell_run_compact and output_read to fetch exact captured bytes without rerunning a command.";
const MAX_INPUT: usize = 16 * 1024 * 1024;

fn json_line(value: &Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)
}

fn save_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

pub(super) fn run(args: impl Iterator<Item = OsString>) -> Result<u8, Box<dyn std::error::Error>> {
    let mut args = args.peekable();
    let action = args.next();
    match action.as_deref().and_then(OsStr::to_str) {
        None | Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(0)
        }
        Some("config") if args.peek().is_none() => {
            json_line(&json!({"mcpServers":{"zrun":{
                "command":std::env::current_exe()?, "args":["ai","connect","--stdio"]
            }}}))?;
            Ok(0)
        }
        Some("connect") => {
            crate::mcp::serve(std::env::current_exe()?, args)?;
            Ok(0)
        }
        Some(action @ ("compact" | "run")) => {
            let save = if args.next_if(|arg| arg == "--save").is_some() {
                Some(std::path::PathBuf::from(
                    args.next().ok_or("--save needs a new path")?,
                ))
            } else {
                None
            };
            if action == "compact" {
                if args.next().is_some() {
                    return Err("ai compact reads its input from stdin".into());
                }
                let mut data = Vec::new();
                io::stdin()
                    .lock()
                    .take(MAX_INPUT as u64 + 1)
                    .read_to_end(&mut data)?;
                if data.len() > MAX_INPUT {
                    return Err("ai compact input exceeds 16 MiB".into());
                }
                if let Some(path) = &save {
                    save_bytes(path, &data)?;
                }
                json_line(&project(&data))?;
                return Ok(0);
            }
            args.next_if(|arg| arg == "--");
            let script = args
                .next()
                .ok_or("ai run needs a script argument")?
                .into_string()
                .map_err(|_| "script must be UTF-8")?;
            let mut request = Request::new(script);
            request.args = args
                .map(|arg| {
                    arg.into_string()
                        .map_err(|_| "script arguments must be UTF-8")
                })
                .collect::<Result<Vec<_>, _>>()?
                .into();
            request.validate()?;
            if let Some(dir) = &save {
                DirBuilder::new().mode(0o700).create(dir)?;
            }
            let mut status = 1;
            crate::execute_batch(
                std::env::current_exe()?,
                vec![request],
                false,
                |_, delivery| {
                    let r = &delivery.result;
                    if let Some(dir) = &save {
                        save_bytes(&dir.join("stdout"), &r.stdout)?;
                        save_bytes(&dir.join("stderr"), &r.stderr)?;
                    }
                    status = crate::exit_code(r.exit_code);
                    json_line(&json!({
                        "disposition":delivery.disposition, "exit_code":r.exit_code,
                        "truncated":r.truncated, "error":r.error,
                        "stdout":project(&r.stdout), "stderr":project(&r.stderr),
                        "saved":save
                    }))
                },
            )?;
            Ok(status)
        }
        _ => Err("unknown ai option; run zrun ai --help".into()),
    }
}
