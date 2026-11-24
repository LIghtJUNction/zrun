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
        format!("# zrun:1\nasync action {request}\nawait action\n"),
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
        assert_eq!(output.stdout, b"remote");
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
fn shell_selection_handles_a_path_alias_back_to_zrun() {
    let directory = Directory::new();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_zrun"), directory.0.join("login-alias"))
        .unwrap();
    let output = zrun()
        .env_remove("ZRUN_SHELL")
        .env("PATH", &directory.0)
        .env("SHELL", "login-alias")
        .args(["-c", "printf fallback"])
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
