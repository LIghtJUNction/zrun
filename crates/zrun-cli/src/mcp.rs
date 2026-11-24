use std::future::Future;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::pin::pin;
use std::task::Poll;

use serde_json::{Value, json};
use zrun::Runtime;
use zrun::io::Reactor;

use crate::execution::{Broker, Delivery, Request};
use crate::executor::Executor;

mod http;

pub(crate) const MAX_MESSAGE: usize = 256 * 1024;
const MAX_BATCH_COMMANDS: usize = 16;
const PROTOCOL: &str = "2025-11-25";
const LEGACY_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", PROTOCOL];

pub(crate) fn serve(
    shell: PathBuf,
    args: impl Iterator<Item = std::ffi::OsString>,
) -> io::Result<()> {
    let mut args = args.peekable();
    if args.peek().is_none() || args.peek().is_some_and(|arg| arg == "--stdio") {
        let _ = args.next();
        if args.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected stdio argument",
            ));
        }
        return serve_stdio(shell);
    }

    let mut tcp = None;
    let mut unix = None;
    while let Some(option) = args.next() {
        let value = args.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing transport address")
        })?;
        match option.to_str() {
            Some("--http") if tcp.is_none() => {
                tcp = Some(
                    value
                        .to_str()
                        .and_then(|text| text.parse().ok())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "expected a loopback socket address",
                            )
                        })?,
                );
            }
            Some("--unix") if unix.is_none() => unix = Some(PathBuf::from(value)),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected --http ADDRESS or --unix PATH",
                ));
            }
        }
    }
    http::serve(shell, tcp, unix)
}

fn serve_stdio(shell: PathBuf) -> io::Result<()> {
    let socket = std::env::var_os("ZRUN_SOCKET").map(PathBuf::from);
    let mut runtime = if socket.is_some() {
        Executor::current()
    } else {
        Executor::configured()?
    };
    let reactor = Reactor::new()?;
    let broker = if socket.is_none() {
        Some(Broker::new(
            shell,
            reactor.handle(),
            runtime.timer(),
            &runtime.spawner(),
        )?)
    } else {
        None
    };
    let timer = runtime.timer();
    let mut input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = (&mut input)
            .take((MAX_MESSAGE + 2) as u64)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok(());
        }
        if line.pop() != Some(b'\n') || line.len() > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP stdio frame exceeds the limit or is incomplete",
            ));
        }
        let response = if let Some(path) = &socket {
            runtime.block_on(http::post(reactor.handle(), path, &line, &timer))?
        } else {
            runtime
                .block_on(respond(
                    broker.as_ref().expect("local stdio has a broker"),
                    &line,
                ))
                .map(|value| serde_json::to_vec(&value))
                .transpose()?
        };
        if let Some(message) = response {
            output.write_all(&message)?;
            writeln!(output)?;
            output.flush()?;
        }
    }
}

pub(crate) fn supports_protocol(version: &str) -> bool {
    LEGACY_VERSIONS.contains(&version)
}

pub(crate) async fn respond(broker: &Broker, input: &[u8]) -> Option<Value> {
    let message = match serde_json::from_slice::<Value>(input) {
        Ok(message) => message,
        Err(_) => return Some(rpc_error(Value::Null, -32700, "parse error")),
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(Value::Null, -32600, "invalid request"));
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(Value::Null, -32600, "invalid JSON-RPC version"));
    }
    let id = message.get("id")?;
    if !id.is_string() && !id.is_number() {
        return Some(rpc_error(Value::Null, -32600, "invalid request id"));
    }
    let params = message.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => {
            let version = params.get("protocolVersion").and_then(Value::as_str);
            let version = version
                .filter(|version| supports_protocol(version))
                .unwrap_or(PROTOCOL);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "zrun", "version": env!("CARGO_PKG_VERSION")},
            })
        }
        "ping" => json!({}),
        "tools/list" => tools(),
        "tools/call" => match call(broker, params).await {
            Ok(result) => result,
            Err(message) => return Some(rpc_error(id.clone(), -32602, &message)),
        },
        _ => return Some(rpc_error(id.clone(), -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn request_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["script"],
        "properties": {
            "script": {"type":"string", "maxLength": 65536},
            "cwd": {"type":"string"},
            "timeout_ms": {"type":"integer", "minimum":1, "maximum":3600000, "default":60000},
            "reuse": {
                "type":"object", "additionalProperties": false,
                "description":"Explicitly declare side-effect-free work and identify its inputs. TTL zero coalesces only overlapping calls.",
                "required":["scope", "input_revision", "ttl_ms"],
                "properties":{
                    "scope":{"type":"string", "minLength":1, "maxLength":128},
                    "input_revision":{"type":"string", "minLength":1, "maxLength":256},
                    "ttl_ms":{"type":"integer", "minimum":0, "maximum":3600000}
                }
            }
        }
    })
}

fn tools() -> Value {
    json!({"tools": [
        {
            "name":"shell_run",
            "description":"Run one shell command in the shared zrun executor with bounded capture and timeout.",
            "inputSchema": request_schema()
        },
        {
            "name":"shell_batch",
            "description":"Run a batch of independent commands concurrently and return results in request order.",
            "inputSchema":{
                "type":"object",
                "additionalProperties": false,
                "required":["commands"],
                "properties":{"commands":{"type":"array", "minItems":1, "maxItems":MAX_BATCH_COMMANDS, "items":request_schema()}}
            }
        }
    ]})
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBatch {
    commands: Vec<Request>,
}

async fn call(broker: &Broker, params: &Value) -> Result<Value, String> {
    let args = params.get("arguments").ok_or("missing tool arguments")?;
    let batch = match params.get("name").and_then(Value::as_str) {
        Some("shell_run") => CommandBatch {
            commands: vec![
                serde_json::from_value(args.clone()).map_err(|error| error.to_string())?,
            ],
        },
        Some("shell_batch") => {
            serde_json::from_value(args.clone()).map_err(|error| error.to_string())?
        }
        _ => return Err("unknown tool".into()),
    };
    if batch.commands.is_empty() || batch.commands.len() > MAX_BATCH_COMMANDS {
        return Err(format!("expected 1 to {MAX_BATCH_COMMANDS} commands"));
    }
    for command in &batch.commands {
        command.validate().map_err(|error| error.to_string())?;
    }
    let many = params["name"] == "shell_batch";
    let mut tickets = Vec::with_capacity(batch.commands.len());
    for command in batch.commands {
        tickets.push(broker.submit(command).await);
    }
    let mut records = Vec::with_capacity(tickets.len());
    let mut failed = false;
    for ticket in tickets {
        match ticket {
            Ok(ticket) => match ticket.await {
                Ok(delivery) => {
                    failed |= delivery.result.exit_code != 0;
                    records.push(json!(delivery));
                }
                Err(error) => {
                    failed = true;
                    records.push(json!(Delivery::rejected(io::Error::other(error))));
                }
            },
            Err(error) => {
                failed = true;
                records.push(json!(Delivery::rejected(io::Error::other(error))));
            }
        }
    }
    let value = if many {
        json!({"commands": records})
    } else {
        records.pop().expect("one command")
    };
    let text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    Ok(json!({"content":[{"type":"text","text":text}],"structuredContent":value,"isError":failed}))
}

async fn deadline<F: Future>(
    future: F,
    timer: &zrun::Timer,
    duration: std::time::Duration,
) -> io::Result<F::Output> {
    let mut future = pin!(future);
    let mut deadline = pin!(timer.sleep(duration));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        match deadline.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Err(io::ErrorKind::TimedOut.into())),
            Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error))),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

pub(crate) fn run_remote(
    path: PathBuf,
    mut requests: Vec<Request>,
    mut record: impl FnMut(usize, Delivery) -> io::Result<()>,
) -> io::Result<()> {
    if requests.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch exceeds the command count limit",
        ));
    }
    let cwd = std::env::current_dir()?;
    for request in &mut requests {
        request.validate()?;
        request.cwd = Some(
            request
                .cwd
                .as_deref()
                .map_or_else(|| cwd.clone(), |dir| cwd.join(dir)),
        );
    }
    let mut runtime = Runtime::new();
    let reactor = Reactor::new()?;
    let timer = runtime.timer();
    runtime.block_on(async {
        let mut offset = 0;
        while offset < requests.len() {
            let mut count = MAX_BATCH_COMMANDS.min(requests.len() - offset);
            let payload = loop {
                let request = json!({
                    "jsonrpc":"2.0", "id":offset, "method":"tools/call",
                    "params":{"name":"shell_batch","arguments":{"commands": &requests[offset..offset + count]}}
                });
                let bytes = serde_json::to_vec(&request)?;
                if bytes.len() <= MAX_MESSAGE {
                    break bytes;
                }
                if count == 1 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "encoded request exceeds the socket message limit"));
                }
                count /= 2;
            };
            let response = http::post(reactor.handle(), &path, &payload, &timer).await?
                .ok_or_else(|| io::Error::other("missing command response"))?;
            let mut response: Value = serde_json::from_slice(&response)?;
            if let Some(error) = response.get("error") {
                return Err(io::Error::other(format!("MCP command request failed: {error}")));
            }
            let records: Vec<Delivery> = serde_json::from_value(
                response["result"]["structuredContent"]["commands"].take()
            )?;
            if records.len() != count || response.get("id").and_then(Value::as_u64) != Some(offset as u64) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected MCP command result"));
            }
            for (index, delivery) in records.into_iter().enumerate() {
                record(offset + index, delivery)?;
            }
            offset += count;
        }
        Ok(())
    })
}
