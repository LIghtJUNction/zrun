use std::future::Future;
use std::io::{self, BufRead, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::execution::{Broker, Delivery, Request};
use crate::executor::Executor;

mod http;
mod reply;

use self::reply::Reply;

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
        match option.to_str() {
            Some("--http") if tcp.is_none() => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing HTTP address")
                })?;
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
            Some("--unix" | "--socket") if unix.is_none() => {
                let path = if args
                    .peek()
                    .is_some_and(|value| !value.as_encoded_bytes().starts_with(b"-"))
                {
                    args.next().map(PathBuf::from)
                } else {
                    None
                };
                unix = Some(path);
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected --http ADDRESS or --unix [PATH]",
                ));
            }
        }
    }
    let unix = unix
        .map(|path| path.map_or_else(http::default_socket_path, Ok))
        .transpose()?;
    http::serve(shell, tcp, unix)
}

fn serve_stdio(shell: PathBuf) -> io::Result<()> {
    let socket = std::env::var_os("ZRUN_SOCKET").map(PathBuf::from);
    let mut runtime = if socket.is_some() {
        Executor::current()?
    } else {
        Executor::configured()?
    };
    let broker = if socket.is_none() {
        Some(Broker::new(
            shell,
            runtime.io(),
            runtime.timer(),
            &runtime.spawner(),
        )?)
    } else {
        None
    };
    let timer = runtime.timer();
    let io = runtime.io().clone();
    let mut client = socket
        .as_deref()
        .map(|path| http::UnixClient::new(&io, path, &timer, true));
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
        if let Some(client) = &mut client {
            let Some(message) = runtime.block_on(client.post(&line))?? else {
                continue;
            };
            output.write_all(&message)?;
        } else {
            let Some(response) = runtime.block_on(respond(
                broker.as_ref().expect("local stdio has a broker"),
                serde_json::from_slice(&line),
                http::IngressCharge::default(),
            ))??
            else {
                continue;
            };
            for chunk in response.chunks() {
                output.write_all(chunk)?;
            }
        }
        writeln!(output)?;
        output.flush()?;
    }
}

pub(crate) fn supports_protocol(version: &str) -> bool {
    LEGACY_VERSIONS.contains(&version)
}

async fn respond(
    broker: &Broker,
    message: serde_json::Result<Value>,
    ingress: http::IngressCharge<'_>,
) -> serde_json::Result<Option<Reply>> {
    let message = match message {
        Ok(message) => message,
        Err(_) => return rpc_error(Value::Null, -32700, "parse error"),
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return rpc_error(Value::Null, -32600, "invalid request");
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return rpc_error(Value::Null, -32600, "invalid JSON-RPC version");
    }
    let Some(id) = message.get("id") else {
        return Ok(None);
    };
    if !id.is_string() && !id.is_number() {
        return rpc_error(Value::Null, -32600, "invalid request id");
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
        "tools/call" => {
            let (batch, commands) = match parse_call(params) {
                Ok(call) => call,
                Err(error) => return rpc_error(id.clone(), -32602, &error),
            };
            let id = id.clone();
            drop(message);
            // Reserve the full batch before dispatch. Partial reservations
            // could leave several batches each waiting on the others to finish.
            let mut capture = broker.reserve_capture(commands.len()).await;
            let records = execute_call(broker, commands, ingress).await;
            capture.shrink_to(
                records
                    .iter()
                    .map(|item| item.result.capture_capacity())
                    .sum(),
            );
            let reply = Reply::tool(&id, &records, batch, broker.output_budget()).await;
            drop(records);
            drop(capture);
            return reply.map(Some);
        }
        _ => return rpc_error(id.clone(), -32601, "method not found"),
    };
    Reply::json(&json!({"jsonrpc": "2.0", "id": id, "result": result})).map(Some)
}

fn rpc_error(id: Value, code: i32, message: &str) -> serde_json::Result<Option<Reply>> {
    Reply::json(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        .map(Some)
}

fn request_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["script"],
        "properties": {
            "script": {"type":"string", "maxLength": 65536},
            "cwd": {"type":"string"},
            "args": {"type":"array", "maxItems":128, "items":{"type":"string"}},
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

fn parse_call(params: &Value) -> Result<(bool, Vec<Request>), String> {
    let args = params.get("arguments").ok_or("missing tool arguments")?;
    let batch = match params.get("name").and_then(Value::as_str) {
        Some("shell_run") => CommandBatch {
            commands: vec![Deserialize::deserialize(args).map_err(|error| error.to_string())?],
        },
        Some("shell_batch") => Deserialize::deserialize(args).map_err(|error| error.to_string())?,
        _ => return Err("unknown tool".into()),
    };
    if batch.commands.is_empty() || batch.commands.len() > MAX_BATCH_COMMANDS {
        return Err(format!("expected 1 to {MAX_BATCH_COMMANDS} commands"));
    }
    for command in &batch.commands {
        command.validate().map_err(|error| error.to_string())?;
    }
    Ok((params["name"] == "shell_batch", batch.commands))
}

async fn execute_call(
    broker: &Broker,
    commands: Vec<Request>,
    ingress: http::IngressCharge<'_>,
) -> Vec<Delivery> {
    let tickets = broker.submit_batch(commands).await;
    drop(ingress);
    let mut records = Vec::with_capacity(tickets.len());
    for ticket in tickets {
        let delivery = match ticket {
            Ok(ticket) => ticket.await.map_err(io::Error::other),
            Err(error) => Err(error),
        };
        records.push(delivery.unwrap_or_else(Delivery::rejected));
    }
    records
}

async fn deadline<F: Future>(
    future: F,
    timer: &zrun::Timer,
    duration: Duration,
) -> io::Result<F::Output> {
    deadline_at(future, timer, Instant::now() + duration).await
}

async fn deadline_at<F: Future>(
    future: F,
    timer: &zrun::Timer,
    expires: Instant,
) -> io::Result<F::Output> {
    timer
        .timeout_at(expires, future)
        .await
        .map_err(|error| match error {
            zrun::TimeoutError::Elapsed => io::ErrorKind::TimedOut.into(),
            zrun::TimeoutError::Timer(error) => io::Error::other(error),
        })
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
    let window = match std::env::var_os("ZRUN_BATCH_WINDOW") {
        None => 1,
        Some(value) => value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|window| (1..=8).contains(window))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ZRUN_BATCH_WINDOW must be 1..8",
                )
            })?,
    };
    // Validate wire sizes for the entire input before a broker can observe
    // any command. Release each request as soon as its payload owns the data.
    let mut requests = requests.into_iter();
    let mut offset = 0;
    let mut batches = Vec::new();
    while requests.len() != 0 {
        let (payload, count) = encode_remote_batch(offset, requests.as_slice())?;
        batches.push((offset, count, payload));
        requests.nth(count - 1);
        offset += count;
    }
    let mut batches = batches.into_iter();
    let mut runtime = Executor::current()?;
    let io = runtime.io().clone();
    let timer = runtime.timer();
    let spawner = runtime.spawner();
    let pool = std::sync::Arc::new(RemotePool::default());
    let path = std::sync::Arc::new(path);
    runtime.block_on(async {
        let mut active = std::collections::VecDeque::with_capacity(window);
        while batches.len() != 0 || !active.is_empty() {
            while active.len() < window {
                let Some((id, count, payload)) = batches.next() else {
                    break;
                };
                let (pool, io, path, timer) =
                    (pool.clone(), io.clone(), path.clone(), timer.clone());
                let job = spawner
                    .spawn(
                        async move { pool.exchange(&io, &path, &timer, &payload, id, count).await },
                    )
                    .map_err(io::Error::other)?;
                active.push_back((id, job));
            }
            let (id, job) = active
                .pop_front()
                .expect("at least one active remote batch");
            for (index, delivery) in job
                .await
                .map_err(io::Error::other)??
                .into_iter()
                .enumerate()
            {
                record(id + index, delivery)?;
            }
        }
        Ok(())
    })?
}

fn encode_remote_batch(offset: usize, requests: &[Request]) -> io::Result<(Vec<u8>, usize)> {
    let mut count = MAX_BATCH_COMMANDS.min(requests.len());
    loop {
        let bytes = remote_payload(offset, &requests[..count])?;
        if bytes.len() <= MAX_MESSAGE {
            return Ok((bytes, count));
        }
        if count == 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "encoded request exceeds the socket message limit",
            ));
        }
        count /= 2;
    }
}

#[derive(Serialize)]
struct RemoteCall<'a> {
    jsonrpc: &'static str,
    id: usize,
    method: &'static str,
    params: RemoteParams<'a>,
}

#[derive(Serialize)]
struct RemoteParams<'a> {
    name: &'static str,
    arguments: RemoteCommands<'a>,
}

#[derive(Serialize)]
struct RemoteCommands<'a> {
    commands: &'a [Request],
}

fn remote_payload(id: usize, commands: &[Request]) -> io::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&RemoteCall {
        jsonrpc: "2.0",
        id,
        method: "tools/call",
        params: RemoteParams {
            name: "shell_batch",
            arguments: RemoteCommands { commands },
        },
    })?)
}

#[derive(Deserialize)]
struct RemoteReply {
    id: Option<u64>,
    result: Option<RemoteResult>,
    error: Option<Value>,
}

#[derive(Deserialize)]
struct RemoteResult {
    #[serde(rename = "structuredContent")]
    content: RemoteDeliveries,
}

#[derive(Deserialize)]
struct RemoteDeliveries {
    commands: Vec<Delivery>,
}

async fn remote_records(
    client: &mut http::UnixClient<'_>,
    payload: &[u8],
    id: usize,
    count: usize,
) -> io::Result<Vec<Delivery>> {
    let body = client
        .post(payload)
        .await?
        .ok_or_else(|| io::Error::other("missing command response"))?;
    let response: RemoteReply = serde_json::from_slice(&body)?;
    if let Some(error) = response.error {
        return Err(io::Error::other(format!(
            "MCP command request failed: {error}"
        )));
    }
    let records = response
        .result
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing MCP command result"))?
        .content
        .commands;
    if records.len() != count || response.id != Some(id as u64) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected MCP command result",
        ));
    }
    Ok(records)
}

#[derive(Default)]
pub(crate) struct RemotePool {
    idle: std::sync::Mutex<Vec<http::IdleStream>>,
}

impl RemotePool {
    pub(crate) async fn run_one(
        &self,
        io: &zrun::io::Handle,
        path: &std::path::Path,
        request: Request,
        timer: &zrun::Timer,
    ) -> io::Result<Delivery> {
        let payload = remote_payload(0, &[request])?;
        let mut records = self.exchange(io, path, timer, &payload, 0, 1).await?;
        Ok(records.remove(0))
    }

    async fn exchange(
        &self,
        io: &zrun::io::Handle,
        path: &std::path::Path,
        timer: &zrun::Timer,
        payload: &[u8],
        id: usize,
        count: usize,
    ) -> io::Result<Vec<Delivery>> {
        const MAX_IDLE: usize = 8;

        let mut client = http::UnixClient::new(io, path, timer, true);
        client.saved = self.idle.lock().unwrap().pop();
        let records = remote_records(&mut client, payload, id, count).await?;
        if let Some(stream) = client.saved.take() {
            let mut idle = self.idle.lock().unwrap();
            if idle.len() < MAX_IDLE {
                idle.push(stream);
            }
        }
        Ok(records)
    }
}
