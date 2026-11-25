#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

fn zrun() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zrun"));
    command
        .env("ZRUN_SHELL", "/bin/sh")
        .env_remove("ZRUN_SOCKET")
        .env_remove("ZRUN_MCP_TOKEN")
        .env_remove("ZRUN_POLICY")
        .env_remove("ZRUN_WORKERS")
        .env_remove("ZRUN_IO_DRIVER")
        .env_remove("ZRUN_BATCH_WINDOW")
        .env_remove("ZRUN_PLUGIN");
    command
}

fn process_input(mut command: Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zrun");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input.as_bytes())
        .expect("write request input");
    child.wait_with_output().expect("read zrun output")
}

fn rpc(id: u32, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
}

fn call(id: u32, argument: Value) -> Value {
    rpc(
        id,
        "tools/call",
        json!({"name":"shell_run","arguments":argument}),
    )
}

fn reusable() -> Value {
    json!({"script":"sleep 0.05; printf reused", "reuse":{"scope":"read-only", "input_revision":"fixture-v1","ttl_ms":10000}})
}

fn lines(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("expected a JSONL response"))
        .collect()
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("zrun-test-{}-{id}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create a private fixture directory: {error}"),
            }
        }
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(command: &mut Command) -> (Server, BufReader<std::process::ChildStderr>) {
    let mut child = command
        .stderr(Stdio::piped())
        .spawn()
        .expect("start zrun daemon");
    let stderr = child.stderr.take().expect("server stderr pipe");
    (Server(child), BufReader::new(stderr))
}

fn startup(stderr: &mut impl BufRead) -> String {
    let mut line = String::new();
    assert_ne!(stderr.read_line(&mut line).expect("read server address"), 0);
    line.trim_end().to_owned()
}

fn post(
    mut socket: impl Read + Write,
    host: &str,
    authorization: &str,
    request: &Value,
) -> (u16, Value) {
    let body = serde_json::to_string(request).expect("serialize test request");
    write!(
        socket,
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{authorization}\r\n{body}",
        body.len(),
    ).expect("send MCP request");
    let mut reply = String::new();
    socket
        .read_to_string(&mut reply)
        .expect("read MCP response");
    let (head, body) = reply.split_once("\r\n\r\n").expect("complete HTTP header");
    let code = head
        .split_whitespace()
        .nth(1)
        .expect("HTTP status")
        .parse()
        .expect("numeric HTTP status");
    (
        code,
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body).expect("valid MCP response")
        },
    )
}

#[test]
fn shell_keeps_the_session_and_run_preserves_exit_and_binary_output() {
    let output = zrun()
        .args(["-c", "cd /; mark=shared; printf '%s:%s' $PWD $mark"])
        .output()
        .expect("run a shell script");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"/:shared");
    let output = zrun()
        .args(["run", "printf '\\377'; printf failed >&2; exit 7"])
        .output()
        .expect("run a managed command");
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, [255]);
    assert_eq!(output.stderr, b"failed");
}

#[test]
fn batch_coalesces_only_opted_in_calls_and_separates_binary_and_timeout() {
    let job = reusable();
    let finite = json!({"script":"sleep 2 & wait","timeout_ms":30});
    let binary = json!({"script":"printf '\\377'"});
    let side_effect_free_but_undeclared = json!({"script":"printf live"});
    let input = format!(
        "{job}\n{job}\n{finite}\n{binary}\n{side_effect_free_but_undeclared}\n{side_effect_free_but_undeclared}\n"
    );
    let output = process_input(
        {
            let mut command = zrun();
            command.arg("batch");
            command
        },
        &input,
    );
    let mut batch = lines(&output);
    batch.sort_by_key(|line| line["index"].as_u64());
    assert_eq!(batch.len(), 6);
    assert_eq!(batch[0]["disposition"], "executed");
    assert_eq!(batch[1]["disposition"], "coalesced");
    assert_eq!(batch[0]["result"]["stdout"]["data"], "reused");
    assert_eq!(batch[1]["result"], batch[0]["result"]);
    assert_eq!(batch[2]["result"]["exit_code"], 124);
    assert_eq!(
        batch[3]["result"]["stdout"],
        json!({"encoding":"base64","data":"/w=="})
    );
    assert_eq!(batch[4]["disposition"], "executed");
    assert_eq!(batch[5]["disposition"], "executed");
}

#[test]
fn tcp_unix_stdio_and_cli_share_one_cache_and_keep_tcp_authenticated() {
    let directory = Directory::new();
    let sock = directory.0.join("zrun.sock");
    let token = "test-authorization-secret-0123456789";
    let (_server, mut stderr) = start(
        zrun()
            .args(["mcp", "--http", "127.0.0.1:0", "--unix"])
            .arg(&sock)
            .env("ZRUN_MCP_TOKEN", token)
            .env("ZRUN_WORKERS", "4"),
    );
    let line = startup(&mut stderr);
    let addr = line
        .strip_prefix("zrun: http://")
        .and_then(|address| address.strip_suffix("/mcp"))
        .expect("bound TCP endpoint");
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let request = call(2, reusable());
    let connect_tcp = || {
        let tcp = TcpStream::connect(addr).expect("connect over TCP");
        tcp.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture deadline");
        tcp
    };
    assert_eq!(post(connect_tcp(), addr, "", &request).0, 401);
    assert_eq!(
        post(
            connect_tcp(),
            "foreign.example",
            &format!("Authorization: Bearer {token}\r\n"),
            &request
        )
        .0,
        403
    );
    let (status, tcp) = post(
        connect_tcp(),
        addr,
        &format!("Authorization: Bearer {token}\r\n"),
        &request,
    );
    assert_eq!(status, 200);
    assert_eq!(
        tcp["result"]["structuredContent"]["disposition"],
        "executed"
    );

    let unix = UnixStream::connect(&sock).expect("connect over Unix");
    unix.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set fixture deadline");
    let (status, unix) = post(unix, "zrun", "", &request);
    assert_eq!(status, 200);
    assert_eq!(unix["result"]["structuredContent"]["disposition"], "cached");

    // Raw captures are bearer capabilities local to the listener that issued them.
    let compact = rpc(
        3,
        "tools/call",
        json!({"name":"shell_run_compact","arguments":reusable()}),
    );
    let authorized = format!("Authorization: Bearer {token}\r\n");
    let (status, reply) = post(connect_tcp(), addr, &authorized, &compact);
    assert_eq!(status, 200);
    let read = rpc(
        4,
        "tools/call",
        json!({"name":"output_read","arguments":{
            "output_id":reply["result"]["structuredContent"]["output_id"],"stream":"stdout"
        }}),
    );
    assert_eq!(post(connect_tcp(), addr, "", &read).0, 401);
    let (_, page) = post(connect_tcp(), addr, &authorized, &read);
    assert_eq!(page["result"]["structuredContent"]["data"], "cmV1c2Vk");
    let other_listener = UnixStream::connect(&sock).unwrap();
    other_listener
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (_, unknown) = post(other_listener, "zrun", "", &read);
    assert_eq!(unknown["result"]["isError"], true);

    let output = process_input(
        {
            let mut client = zrun();
            client.args(["mcp", "--stdio"]).env("ZRUN_SOCKET", &sock);
            client
        },
        &format!("{request}\n"),
    );
    assert_eq!(
        lines(&output)[0]["result"]["structuredContent"]["disposition"],
        "cached"
    );
    let output = process_input(
        {
            let mut client = zrun();
            client.arg("batch").env("ZRUN_SOCKET", &sock);
            client
        },
        &format!("{}\n", reusable()),
    );
    assert_eq!(lines(&output)[0]["disposition"], "cached");
}

#[test]
fn batch_input_accepts_crlf_and_final_unterminated_record() {
    let mut command = zrun();
    command.arg("batch");
    let output = process_input(
        command,
        " \r\n{\"script\":\"printf first\"}\r\n{\"script\":\"printf second\"}",
    );
    let records = lines(&output);
    assert_eq!(records.len(), 2);
    for record in records {
        let text = if record["index"] == 0 {
            "first"
        } else {
            "second"
        };
        assert_eq!(record["result"]["stdout"]["data"], text);
    }
}

#[test]
fn batch_input_keeps_the_total_limit_after_a_parse_error() {
    let directory = Directory::new();
    let mut command = zrun();
    command.current_dir(&directory.0).arg("batch");
    let mut input = format!(
        "{{\"script\":\"touch executed\"}}\ninvalid JSON\n{}",
        " \n".repeat(1024 * 1024)
    );
    input.truncate(2 * 1024 * 1024 + 1);
    let output = process_input(command, &input);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("input exceeds the byte limit"));
    assert!(!directory.0.join("executed").exists());
}

#[test]
fn local_batch_delivers_each_result_without_waiting_for_the_next_command() {
    let directory = Directory::new();
    let mut child = zrun()
        .arg("batch")
        .current_dir(&directory.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            json!({"script":"while [ ! -f release ]; do sleep .01; done","timeout_ms":5000})
        )
        .unwrap();
        writeln!(input, "{}", json!({"script":"printf ready"})).unwrap();
    }
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    output.read_line(&mut first).unwrap();
    fs::write(directory.0.join("release"), b"").unwrap();
    let mut second = String::new();
    output.read_line(&mut second).unwrap();
    assert!(child.wait().unwrap().success());
    let first: Value = serde_json::from_str(&first).unwrap();
    let second: Value = serde_json::from_str(&second).unwrap();
    assert_eq!(first["index"], 1);
    assert_eq!(first["result"]["stdout"]["data"], "ready");
    assert_eq!(second["index"], 0);
    assert_eq!(second["result"]["exit_code"], 0);
}

#[test]
fn worker_io_broker_handles_unix_requests_pipes_and_deadlines() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(
        zrun()
            .env("ZRUN_WORKERS", "4")
            .env("ZRUN_IO_DRIVER", "worker")
            .args(["mcp", "--unix"])
            .arg(&socket),
    );
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let mut client = zrun();
    client.arg("batch").env("ZRUN_SOCKET", &socket);
    let records = lines(&process_input(
        client,
        "{\"script\":\"printf output; printf error >&2\"}\n{\"script\":\"sleep 5\",\"timeout_ms\":100}\n",
    ));
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["result"]["stdout"]["data"], "output");
    assert_eq!(records[0]["result"]["stderr"]["data"], "error");
    assert_eq!(records[1]["result"]["exit_code"], 124);
}

#[test]
fn remote_batch_rejects_late_wire_oversize_before_starting_any_command() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let input = format!(
        "{}{}\n",
        format!("{}\n", json!({"script":"touch executed"})).repeat(16),
        json!({"script":"\u{1}".repeat(65_536)})
    );
    let mut client = zrun();
    client
        .arg("batch")
        .current_dir(&directory.0)
        .env("ZRUN_SOCKET", &socket);
    let output = process_input(client, &input);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("encoded request exceeds"));
    assert!(!directory.0.join("executed").exists());
}

#[test]
fn remote_batch_window_overlaps_requests_and_preserves_output_order() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let input = (0..17)
        .map(|index| {
            let script = match index {
                0 => "while [ ! -f ready ]; do sleep .01; done",
                16 => "touch ready",
                _ => "true",
            };
            format!("{}\n", json!({"script":script,"timeout_ms":3000}))
        })
        .collect::<String>();
    let mut client = zrun();
    client
        .arg("batch")
        .current_dir(&directory.0)
        .env("ZRUN_SOCKET", &socket)
        .env("ZRUN_BATCH_WINDOW", "2");
    let records = lines(&process_input(client, &input));
    assert_eq!(records.len(), 17);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["index"], index);
        assert_eq!(record["result"]["exit_code"], 0, "{record}");
    }
}

#[test]
fn remote_batch_ready_refills_the_window_behind_a_blocked_first_batch() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let input = (0..33)
        .map(|index| {
            let script = match index {
                0 => "while [ ! -f third ]; do sleep .01; done",
                32 => "touch third",
                _ => "true",
            };
            format!("{}\n", json!({"script":script,"timeout_ms":5000}))
        })
        .collect::<String>();
    let mut client = zrun();
    client
        .args(["batch", "--ready"])
        .current_dir(&directory.0)
        .env("ZRUN_SOCKET", &socket)
        .env("ZRUN_BATCH_WINDOW", "2");
    let records = lines(&process_input(client, &input));
    assert_eq!(records.len(), 33);
    assert_eq!(records[0]["index"], 16);
    let mut indexes = Vec::new();
    for record in &records {
        indexes.push(record["index"].as_u64().unwrap());
        assert_eq!(record["result"]["exit_code"], 0, "{record}");
    }
    indexes.sort_unstable();
    assert!(indexes.into_iter().eq(0..33));
}

#[test]
fn local_stdio_negotiates_tools_and_zguard_checks_before_reading_cache() {
    let directory = Directory::new();
    let policy = directory.0.join("policy.json");
    fs::write(&policy, br#"{"max_calls":1}"#).expect("write fixture guard policy");
    let init = rpc(
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    );
    let list = rpc(2, "tools/list", json!({}));
    let command = call(3, reusable());
    let retry = call(4, reusable());
    let output = process_input(
        {
            let mut command = zrun();
            command.args(["mcp", "--stdio"]).env("ZRUN_POLICY", policy);
            command
        },
        &format!("{init}\n{list}\n{command}\n{retry}\n"),
    );
    let lines = lines(&output);
    assert_eq!(lines[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(lines[1]["result"]["tools"][1]["name"], "shell_batch");
    assert_eq!(
        lines[2]["result"]["structuredContent"]["disposition"],
        "executed"
    );
    assert_eq!(
        lines[3]["result"]["structuredContent"]["disposition"],
        "rejected"
    );
    assert_eq!(lines[3]["result"]["isError"], true);
    assert!(
        lines[3]["result"]["structuredContent"]["result"]["error"]
            .as_str()
            .expect("guard denial")
            .contains("CallLimit")
    );
}

#[test]
fn chunked_socket_requests_and_graceful_shutdown() {
    let directory = Directory::new();
    let sock = directory.0.join("zrun.sock");
    let (mut server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&sock));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let mut connection = UnixStream::connect(&sock).expect("connect to zrun socket");
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set fixture deadline");
    connection.write_all(b"POST /mcp HTTP/1.1\r\nHost: zrun\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n")
        .expect("send chunked HTTP headers");
    let message = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for chunk in message.chunks(7) {
        write!(connection, "{:x}\r\n", chunk.len()).expect("send chunk length");
        connection.write_all(chunk).expect("send chunk");
        connection.write_all(b"\r\n").expect("send chunk boundary");
    }
    connection
        .write_all(b"0\r\nX-Test: yes\r\n\r\n")
        .expect("send end of request");
    let mut result = String::new();
    connection
        .read_to_string(&mut result)
        .expect("read HTTP response");
    assert!(result.starts_with("HTTP/1.1 200 OK\r\n"), "{result}");
    assert!(
        result.ends_with(r#"{"id":1,"jsonrpc":"2.0","result":{}}"#),
        "{result}"
    );

    let pid = i32::try_from(server.0.id()).expect("Linux child PID fits i32");
    // SAFETY: this is the live server child owned by the fixture.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert!(server.0.wait().expect("wait for daemon shutdown").success());
    assert!(!sock.exists());
}

#[test]
fn default_socket_uses_the_private_xdg_directory() {
    let directory = Directory::new();
    let service = directory.0.join("zrun");
    let socket = service.join("mcp.sock");
    let (mut server, mut stderr) = start(
        zrun()
            .env("XDG_RUNTIME_DIR", &directory.0)
            .args(["mcp", "--socket"]),
    );
    assert_eq!(
        startup(&mut stderr),
        format!("zrun: unix:{}", socket.display())
    );
    assert_eq!(
        service.metadata().unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        socket.symlink_metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );

    let duplicate = zrun()
        .env("XDG_RUNTIME_DIR", &directory.0)
        .args(["mcp", "--unix"])
        .output()
        .expect("try to start a second daemon");
    assert!(!duplicate.status.success());
    let connection = UnixStream::connect(&socket).expect("original socket still accepts clients");
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (status, reply) = post(connection, "zrun", "", &rpc(1, "ping", Value::Null));
    assert_eq!(status, 200);
    assert_eq!(reply["id"], 1);

    let pid = i32::try_from(server.0.id()).expect("Linux child PID fits i32");
    // SAFETY: this is the live server child owned by the fixture.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert!(server.0.wait().expect("wait for daemon shutdown").success());
    assert!(!socket.exists());
}

#[test]
fn default_socket_refuses_a_shared_runtime_directory() {
    let directory = Directory::new();
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
    let output = zrun()
        .env("XDG_RUNTIME_DIR", &directory.0)
        .args(["mcp", "--unix"])
        .output()
        .expect("reject insecure runtime directory");
    assert!(!output.status.success());
    assert!(!directory.0.join("zrun").exists());
}

#[test]
fn mcp_batch_preserves_order_binary_output_and_escaped_content() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let mut request = rpc(
        7,
        "tools/call",
        json!({"name": "shell_batch", "arguments": {"commands": [
            {"script": "sleep 0.01; printf '%s\\n' 'a\"b'"},
            {"script": "printf '\\377' >&2; exit 7"}
        ]}}),
    );
    let id = "escaped-\"-测试";
    request["id"] = json!(id);
    let connection = UnixStream::connect(&socket).expect("connect to zrun daemon");
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set fixture deadline");
    let (status, reply) = post(connection, "zrun", "", &request);
    assert_eq!(status, 200);
    assert_eq!(reply["id"], id);
    let reply = &reply["result"];
    assert_eq!(reply["isError"], true);
    let text: Value = serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, reply["structuredContent"]);
    let commands = text["commands"].as_array().unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(
        commands[0]["result"]["stdout"],
        json!({"encoding": "utf8", "data": "a\"b\n"})
    );
    assert_eq!(
        commands[1]["result"]["stderr"],
        json!({"encoding": "base64", "data": "/w=="})
    );
    assert_eq!(commands[1]["result"]["exit_code"], 7);
}

#[test]
fn zrun_file_without_header_preserves_shell_arguments_and_exit_status() {
    let dir = Directory::new();
    let path = dir.0.join("plain.zrun");
    fs::write(
        &path,
        "#!/bin/sh\nfor arg in \"$@\"; do printf '<%s>' \"$arg\"; done\nexit 7\n",
    )
    .unwrap();
    let output = zrun()
        .args([path.as_os_str(), "a b".as_ref(), "c".as_ref()])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"<a b><c>");
}

#[test]
fn plain_script_uses_its_shebang_interpreter_and_optional_argument() {
    let dir = Directory::new();
    let path = dir.0.join("interpreter.zrun");
    fs::write(
        &path,
        "#!/bin/sh -e\nprintf '<%s>' \"$1\"\nfalse\nprintf ignored\n",
    )
    .unwrap();
    let output = zrun()
        .env("ZRUN_SHELL", "/bin/false")
        .arg(path)
        .arg("two words")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"<two words>");
}

#[test]
fn plain_script_with_zrun_shebang_uses_native_shell() {
    let dir = Directory::new();
    let path = dir.0.join("wrapper.zrun");
    fs::write(&path, "#!/usr/bin/env -S zrun\nprintf wrapped\n").unwrap();
    let output = zrun()
        .env("ZRUN_SHELL", "/missing-backend")
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"wrapped");
}

#[test]
fn end_of_options_preserves_native_and_plain_dash_prefixed_scripts() {
    let dir = Directory::new();
    fs::write(
        dir.0.join("-native.zrun"),
        "# zrun:1\nasync job -- printf '<%s>' \"$1\"\nawait job\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("-plain.zrun"),
        "#!/bin/sh\nprintf '<%s>' \"$1\"\n",
    )
    .unwrap();
    for file in ["-native.zrun", "-plain.zrun"] {
        let output = zrun()
            .current_dir(&dir.0)
            .args(["--", file, "-argument"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"<-argument>");
    }
}

#[test]
fn native_script_jobs_overlap_and_emit_in_await_order() {
    let dir = Directory::new();
    let path = dir.0.join("overlap.zrun");
    fs::write(
        &path,
        r#"# zrun:1
async first {"script":"touch first; while [ ! -f second ]; do sleep .01; done; printf first; exit 17","timeout_ms":5000}
async second {"script":"touch second; while [ ! -f first ]; do sleep .01; done; printf second; exit 19","timeout_ms":5000}
await second first
"#,
    ).unwrap();
    let output = zrun().current_dir(&dir.0).arg(path).output().unwrap();
    assert_eq!(output.status.code(), Some(19), "{:?}", output);
    assert_eq!(output.stdout, b"secondfirst");
}

#[test]
fn native_inline_commands_preserve_shell_syntax_and_arguments() {
    let dir = Directory::new();
    let path = dir.0.join("inline.zrun");
    fs::write(
        &path,
        r#"# zrun:1
async prepare -- printf '%s' "$(printf '%s' "$1")" | tr '[:lower:]' '[:upper:]' > result
await prepare
shell -- cat result; printf '|%s' --flag
"#,
    )
    .unwrap();
    let output = zrun()
        .current_dir(&dir.0)
        .arg(path)
        .arg("two words")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"TWO WORDS|--flag");
}

#[test]
fn native_await_delivers_stdout_without_a_newline_before_the_next_wait() {
    let dir = Directory::new();
    let path = dir.0.join("flush.zrun");
    fs::write(
        &path,
        r#"# zrun:1
async ready -- printf ready
async blocked {"script":"while [ ! -f release ]; do sleep .01; done","timeout_ms":5000}
await ready blocked
"#,
    )
    .unwrap();
    let mut child = zrun()
        .current_dir(&dir.0)
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0; 5];
    child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
    fs::write(dir.0.join("release"), b"").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(&ready, b"ready");
}

#[test]
fn native_await_ready_delivers_a_later_job_before_a_blocked_job() {
    let dir = Directory::new();
    let path = dir.0.join("ready.zrun");
    fs::write(
        &path,
        r#"# zrun:1
async alpha {"script":"while [ ! -f release ]; do sleep .01; done; printf done","timeout_ms":5000}
async beta -- printf ready
await --ready
async beta -- printf again
await --ready
"#,
    )
    .unwrap();
    let mut child = zrun()
        .current_dir(&dir.0)
        .arg(path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut ready = [0; 5];
    output.read_exact(&mut ready).unwrap();
    fs::write(dir.0.join("release"), b"").unwrap();
    let mut tail = String::new();
    output.read_to_string(&mut tail).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(&ready, b"ready");
    assert_eq!(tail, "doneagain");
}

#[test]
fn native_script_reuses_an_explicit_key_and_validates_before_running() {
    let dir = Directory::new();
    let path = dir.0.join("cache.zrun");
    let request = json!({
        "script": "printf x >> count; printf ok",
        "reuse": {"scope":"fixture","input_revision":"v1","ttl_ms":5000}
    });
    fs::write(
        &path,
        format!(
            "# zrun:1\nasync first {request}\nasync second {request}\nawait first\nawait second\n"
        ),
    )
    .unwrap();
    let output = zrun().current_dir(&dir.0).arg(&path).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"okok");
    assert_eq!(fs::read(dir.0.join("count")).unwrap(), b"x");

    fs::write(
        &path,
        "# zrun:1\nshell <<END\ntouch unexpected\nEND\nasync known {\"script\":\"true\"}\nawait known missing\n",
    )
    .unwrap();
    let output = zrun().current_dir(&dir.0).arg(path).output().unwrap();
    assert!(!output.status.success());
    assert!(!dir.0.join("unexpected").exists());
}

#[test]
fn native_remote_script_preflights_wire_sizes_before_submitting() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let path = directory.0.join("oversize.zrun");
    fs::write(
        &path,
        format!(
            "# zrun:1\nshell -- touch executed\nshell {}\n",
            json!({"script":"\u{1}".repeat(65_536)})
        ),
    )
    .unwrap();
    let output = zrun()
        .current_dir(&directory.0)
        .env("ZRUN_SOCKET", &socket)
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("encoded request exceeds"));
    assert!(!directory.0.join("executed").exists());
}

#[test]
fn native_scripts_share_the_unix_broker_across_processes() {
    let directory = Directory::new();
    let sock = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&sock).current_dir("/"));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let file = directory.0.join("remote.zrun");
    let request = json!({
        "script": "printf x >> count; sleep 0.1; printf remote",
        "reuse": {"scope":"remote-fixture", "input_revision":"v1", "ttl_ms":5000}
    });
    fs::write(
        &file,
        format!("# zrun:1\nawait *\nasync zebra {request}\nasync alpha <<END\nprintf second\nEND\nawait *\n"),
    )
    .unwrap();
    let launch = || {
        zrun()
            .current_dir(&directory.0)
            .env("ZRUN_SOCKET", &sock)
            .arg(&file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = launch();
    let second = launch();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(output.stdout, b"remotesecond");
    }
    assert_eq!(fs::read(directory.0.join("count")).unwrap(), b"x");
}

#[test]
fn mcp_batch_authorizes_each_request_before_reuse() {
    let directory = Directory::new();
    let policy = directory.0.join("policy.json");
    fs::write(&policy, br#"{"max_calls":2}"#).unwrap();
    let request = rpc(
        1,
        "tools/call",
        json!({"name":"shell_batch","arguments":{"commands":[reusable(), reusable(), reusable()]}}),
    );
    let mut command = zrun();
    command.args(["mcp", "--stdio"]).env("ZRUN_POLICY", policy);
    let response = process_input(command, &format!("{request}\n"));
    let output = lines(&response);
    let records = output[0]["result"]["structuredContent"]["commands"]
        .as_array()
        .unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["disposition"], "executed");
    assert_eq!(records[1]["disposition"], "coalesced");
    assert_eq!(records[2]["disposition"], "rejected");
    assert!(
        records[2]["result"]["error"]
            .as_str()
            .unwrap()
            .contains("CallLimit")
    );
}

#[test]
fn login_shell_forwards_command_and_exit_status() {
    use std::os::unix::process::CommandExt;

    let output = zrun()
        .arg0("-zrun")
        .args(["-c", "printf '%s' \"$1\"; exit 37", "fixture", "login"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stdout, b"login");
}

#[test]
fn login_shell_passes_the_backend_login_name_as_argv_zero() {
    use std::os::unix::process::CommandExt;

    let output = zrun()
        .arg0("-zrun")
        .args(["--shell", "-c", "printf '%s' \"$0\""])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"-sh");
}

#[test]
fn shell_selection_handles_a_path_alias_back_to_zrun() {
    let directory = Directory::new();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zrun"), directory.0.join("login-alias"))
        .unwrap();
    let output = zrun()
        .env_remove("ZRUN_SHELL")
        .env("PATH", &directory.0)
        .env("SHELL", "login-alias")
        .args(["--shell", "-c", "printf fallback"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fallback");
}

#[test]
fn an_executable_native_script_runs_through_its_shebang() {
    let dir = Directory::new();
    let path = dir.0.join("native.zrun");
    fs::write(
        &path,
        format!(
            "#!{}\n# zrun:1\nasync job <<END\nprintf native; exit 37\nEND\nawait job\n",
            env!("CARGO_BIN_EXE_zrun")
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(path)
        .env("ZRUN_SHELL", "/bin/sh")
        .env_remove("ZRUN_SOCKET")
        .env_remove("ZRUN_POLICY")
        .env_remove("ZRUN_PLUGIN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(output.stdout, b"native");
}

#[test]
fn native_positional_arguments_reach_the_broker_and_distinguish_cache_entries() {
    let directory = Directory::new();
    let socket = directory.0.join("zrun.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&socket));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));
    let path = directory.0.join("args.zrun");
    fs::write(
        &path,
        r#"# zrun:1
async job {"script":"printf x >> count; printf '<%s>' \"$1\"","reuse":{"scope":"args","input_revision":"v1","ttl_ms":10000}}
await *
"#,
    ).unwrap();
    for arg in ["one two", "other' *", "one two"] {
        let output = zrun()
            .current_dir(&directory.0)
            .env("ZRUN_SOCKET", &socket)
            .args([path.as_os_str(), arg.as_ref()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(output.stdout, format!("<{arg}>").as_bytes());
    }
    assert_eq!(fs::read(directory.0.join("count")).unwrap(), b"xx");
}

#[test]
fn login_shell_uses_the_configured_backend_and_env_can_override_it() {
    use std::os::unix::process::CommandExt;

    let dir = Directory::new();
    let backend = dir.0.join("backend");
    fs::write(&backend, "#!/bin/sh\nprintf 'backend:%s' \"$1\"\n").unwrap();
    fs::set_permissions(&backend, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(dir.0.join("zrun")).unwrap();
    fs::write(dir.0.join("zrun/shell"), format!("{}\n", backend.display())).unwrap();
    let launch = || {
        let mut command = zrun();
        command
            .arg0("-zrun")
            .env("XDG_CONFIG_HOME", &dir.0)
            .env("SHELL", env!("CARGO_BIN_EXE_zrun"))
            .env_remove("ZRUN_SHELL")
            .args(["--shell", "-c", "printf explicit"]);
        command
    };
    let configured = launch().output().unwrap();
    assert!(configured.status.success());
    assert_eq!(configured.stdout, b"backend:-l");
    let overridden = launch().env("ZRUN_SHELL", "/bin/sh").output().unwrap();
    assert!(overridden.status.success());
    assert_eq!(overridden.stdout, b"explicit");
}

#[test]
fn unix_mcp_connection_can_reuse_the_stream_for_sequential_requests() {
    let dir = Directory::new();
    let sock = dir.0.join("mcp.sock");
    let (_server, mut stderr) = start(zrun().args(["mcp", "--unix"]).arg(&sock));
    assert!(startup(&mut stderr).starts_with("zrun: unix:"));

    let connection = UnixStream::connect(sock).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(connection);
    for (id, host, expected, keep_open) in [
        (1, "zrun", 200, true),
        (2, "zrun", 200, true),
        (3, "other", 403, false),
    ] {
        check_sequential_post(&mut reader, id, host, "", expected, keep_open);
    }
    let mut remainder = Vec::new();
    reader.read_to_end(&mut remainder).unwrap();
    assert!(remainder.is_empty());
}

fn check_sequential_post(
    reader: &mut BufReader<impl Read + Write>,
    id: u32,
    host: &str,
    authorization: &str,
    expected: u16,
    keep_open: bool,
) {
    let body = serde_json::to_vec(&rpc(id, "tools/list", json!({}))).unwrap();
    let stream = reader.get_mut();
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{authorization}\r\n",
        body.len()
    ).unwrap();
    stream.write_all(&body).unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        line.split_ascii_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap(),
        expected
    );
    let mut length = None;
    let mut connection = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
        if let Some(value) = line.strip_prefix("Connection: ") {
            connection = Some(value.trim().to_string());
        }
    }
    assert_eq!(
        connection.as_deref(),
        Some(if keep_open { "keep-alive" } else { "close" })
    );
    let mut response = vec![0; length.unwrap()];
    reader.read_exact(&mut response).unwrap();
    if expected == 200 {
        assert_eq!(
            serde_json::from_slice::<Value>(&response).unwrap()["id"],
            id
        );
    }
}

#[test]
fn tcp_mcp_keep_alive_rechecks_authentication_and_bounds_requests() {
    let token = "test-authorization-secret-0123456789";
    let (_server, mut stderr) = start(
        zrun()
            .args(["mcp", "--http", "127.0.0.1:0"])
            .env("ZRUN_MCP_TOKEN", token),
    );
    let line = startup(&mut stderr);
    let addr = line
        .strip_prefix("zrun: http://")
        .and_then(|address| address.strip_suffix("/mcp"))
        .expect("bound TCP endpoint");
    let connect = || {
        let stream = TcpStream::connect(addr).expect("connect TCP client");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        BufReader::new(stream)
    };
    let authorization = format!("Authorization: Bearer {token}\r\n");
    let mut reader = connect();
    check_sequential_post(&mut reader, 1, addr, &authorization, 200, true);
    check_sequential_post(&mut reader, 2, addr, &authorization, 200, true);
    check_sequential_post(&mut reader, 3, addr, "", 401, false);
    assert!(reader.fill_buf().unwrap().is_empty());

    let mut reader = connect();
    for id in 1..=64 {
        check_sequential_post(&mut reader, id, addr, &authorization, 200, id < 64);
    }
    assert!(reader.fill_buf().unwrap().is_empty());
}

#[test]
fn program_arguments_are_literal_and_files_still_run_as_scripts() {
    let dir = Directory::new();
    let file = dir.0.join("with space");
    fs::write(&file, "printf '%s' \"$1\"\n").unwrap();
    let name = "with space";
    let listed = zrun()
        .current_dir(&dir.0)
        .args(["ls", name])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(listed.stdout, b"with space\n");
    let literal = "$(printf executed); *";
    for path in ["printf", "/usr/bin/printf"] {
        let printed = zrun().args([path, "%s", literal]).output().unwrap();
        assert!(printed.status.success());
        assert_eq!(printed.stdout, literal.as_bytes());
    }
    let script = zrun()
        .current_dir(&dir.0)
        .args([name, literal])
        .output()
        .unwrap();
    assert!(script.status.success());
    assert_eq!(script.stdout, literal.as_bytes());
    let missing = zrun()
        .arg("zrun-test-missing-command-32f9")
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(127));
}

#[test]
fn compact_mcp_pages_from_the_same_capture_and_enforces_command_policy() {
    use sha2::{Digest, Sha256};

    let directory = Directory::new();
    let policy = directory.0.join("policy.json");
    fs::write(&policy, br#"{"max_calls":2}"#).unwrap();
    let mut child = zrun()
        .args(["mcp", "--stdio"])
        .env("ZRUN_POLICY", policy)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |request: Value| {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        let reply = serde_json::from_str::<Value>(&line).unwrap();
        assert_eq!(reply["id"], request["id"]);
        assert_eq!(reply["jsonrpc"], "2.0");
        if request["method"] == "initialize" {
            writeln!(
                input,
                r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
            )
            .unwrap();
        }
        reply
    };
    let tool = |id: u32, name: &str, args: Value| {
        rpc(id, "tools/call", json!({"name":name,"arguments":args}))
    };
    let initialized = exchange(rpc(
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
    ));
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    let discovery = exchange(rpc(2, "tools/list", json!({})));
    let names: Vec<_> = discovery["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "shell_run",
            "shell_batch",
            "shell_run_compact",
            "output_read"
        ]
    );
    let args = json!({"script":r#"printf 'x\nx\n'; printf '\000\377A' >&2; exit 7"#});
    let reply = exchange(tool(3, "shell_run_compact", args));
    assert_eq!(reply["result"]["isError"], true);
    let result = &reply["result"]["structuredContent"];
    assert_eq!(result["disposition"], "executed");
    assert_eq!(result["exit_code"], 7);
    assert!(result["error"].is_null());
    assert_eq!(result["truncated"], false);
    assert_eq!(result["stdout"]["runs"], json!([{"text":"x\n","count":2}]));
    assert_eq!(result["stderr"]["encoding"], "base64");
    assert_eq!(
        result["stderr"]["sha256"],
        format!("{:x}", Sha256::digest([0, 255, 65]))
    );
    assert_eq!(
        serde_json::from_str::<Value>(reply["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        *result
    );
    let page = |cursor: usize, limit: usize| json!({"output_id":result["output_id"],"stream":"stderr","cursor":cursor,"limit":limit});
    let mut other_session = zrun();
    other_session.args(["mcp", "--stdio"]);
    let lookup = tool(4, "output_read", page(0, 2));
    let unknown = lines(&process_input(other_session, &format!("{lookup}\n")));
    assert_eq!(unknown[0]["result"]["isError"], true);
    let first = exchange(tool(4, "output_read", page(0, 2)));
    let second = exchange(tool(5, "output_read", page(2, 2)));
    let first = &first["result"]["structuredContent"];
    let second = &second["result"]["structuredContent"];
    assert_eq!(first["data"], "AP8=");
    assert_eq!(first["next_cursor"], 2);
    assert_eq!(first["eof"], false);
    assert_eq!(second["data"], "QQ==");
    assert_eq!(second["eof"], true);
    assert_eq!(second["sha256"], first["sha256"]);
    assert_eq!(
        exchange(tool(6, "output_read", page(4, 1)))["result"]["isError"],
        true
    );
    assert_eq!(
        exchange(tool(7, "output_read", page(0, 4097)))["error"]["code"],
        -32602
    );

    let second_run = exchange(tool(8, "shell_run_compact", reusable()));
    assert_eq!(
        second_run["result"]["structuredContent"]["disposition"],
        "executed"
    );
    let rejected = exchange(tool(9, "shell_run_compact", reusable()));
    assert_eq!(rejected["result"]["isError"], true);
    let denied = &rejected["result"]["structuredContent"];
    assert_eq!(denied["disposition"], "rejected");
    assert!(denied["error"].as_str().unwrap().contains("CallLimit"));
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn compact_mcp_preserves_broker_truncation_and_reports_an_authorized_cache_hit() {
    let run = |id: u32, args: Value| {
        rpc(
            id,
            "tools/call",
            json!({"name":"shell_run_compact","arguments":args}),
        )
    };
    let big = run(1, json!({"script":"head -c 70000 /dev/zero"}));
    let first = run(2, reusable());
    let second = run(3, reusable());
    let mut cmd = zrun();
    cmd.args(["mcp", "--stdio"]);
    let rows = lines(&process_input(cmd, &format!("{big}\n{first}\n{second}\n")));
    let result = &rows[0]["result"]["structuredContent"];
    assert_eq!(result["truncated"], true);
    assert_eq!(result["stdout"]["preview_truncated"], true);
    assert_eq!(result["stdout"]["captured_bytes"], 65536);
    assert_eq!(result["stderr"]["captured_bytes"], 0);
    assert_eq!(
        rows[1]["result"]["structuredContent"]["disposition"],
        "executed"
    );
    assert_eq!(
        rows[2]["result"]["structuredContent"]["disposition"],
        "cached"
    );
    assert_ne!(
        rows[1]["result"]["structuredContent"]["output_id"],
        rows[2]["result"]["structuredContent"]["output_id"]
    );
}
