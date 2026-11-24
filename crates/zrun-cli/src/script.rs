use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::execution::{Batch, Delivery, Request, Ticket};

pub(crate) const HEADER: &str = "# zrun:1";

enum Step {
    Spawn(String, Request),
    Await(String),
    Shell(Request),
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn has_native_header(source: &[u8]) -> bool {
    let mut lines = source.split_inclusive(|byte| *byte == b'\n');
    let first = lines.next();
    let header = if first.is_some_and(|line| line.starts_with(b"#!")) {
        lines.next()
    } else {
        first
    };
    header.is_some_and(|line| {
        line.strip_suffix(b"\r\n")
            .or_else(|| line.strip_suffix(b"\n"))
            == Some(HEADER.as_bytes())
    })
}

fn parse_with_args(source: &str, args: Arc<Vec<String>>) -> io::Result<Vec<Step>> {
    let mut lines = source.lines().enumerate();
    let first = lines.next();
    let header = if first.is_some_and(|(_, line)| line.starts_with("#!")) {
        lines.next()
    } else {
        first
    };
    if header.map(|(_, line)| line) != Some(HEADER) {
        return Err(invalid("missing # zrun:1 header"));
    }
    let mut pending = Vec::new();
    let mut steps = Vec::new();
    while let Some((index, text)) = lines.next() {
        let fail = |reason: &str| invalid(format!("line {}: {reason}", index + 1));
        let line = text.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if steps.len() == 8192 {
            return Err(fail("too many script steps"));
        }
        let (operation, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| fail("expected async, await or shell with arguments"))?;
        let rest = rest.trim_start();
        match operation {
            "await" if rest.trim_end() == "*" => {
                if steps.len() + pending.len() > 8192 {
                    return Err(fail("too many script steps"));
                }
                steps.extend(pending.drain(..).map(|name: &str| Step::Await(name.into())));
            }
            "await" => {
                for name in rest.split_whitespace() {
                    let slot = pending
                        .iter()
                        .position(|pending| *pending == name)
                        .ok_or_else(|| fail("await requires the names of pending tasks"))?;
                    pending.remove(slot);
                    if steps.len() == 8192 {
                        return Err(fail("too many script steps"));
                    }
                    steps.push(Step::Await(name.into()));
                }
            }
            "async" | "shell" => {
                let (name, payload) = if operation == "async" {
                    let (name, payload) = rest
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| fail("async requires a name and command"))?;
                    if name.is_empty()
                        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    {
                        return Err(fail("invalid task name"));
                    }
                    if pending.len() == 128 || pending.contains(&name) {
                        return Err(fail("duplicate task name or 128 pending tasks"));
                    }
                    pending.push(name);
                    (Some(name), payload.trim_start())
                } else {
                    (None, rest)
                };
                let mut request = if let Some(tag) = payload.strip_prefix("<<") {
                    let tag = tag.trim_end();
                    if tag.is_empty()
                        || !tag.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    {
                        return Err(fail("invalid shell block delimiter"));
                    }
                    let mut script = String::new();
                    loop {
                        let (_, line) = lines
                            .next()
                            .ok_or_else(|| fail("unterminated shell block"))?;
                        if line == tag {
                            break;
                        }
                        script.push_str(line);
                        script.push('\n');
                    }
                    Request::new(script)
                } else if let Some(command) = payload.strip_prefix("--").filter(|command| {
                    command.is_empty() || command.starts_with(char::is_whitespace)
                }) {
                    let command = command.trim_start();
                    if command.is_empty() {
                        return Err(fail("missing shell command after --"));
                    }
                    Request::new(command.into())
                } else {
                    serde_json::from_str::<Request>(payload)
                        .map_err(|error| fail(&error.to_string()))?
                };
                if request.args.is_empty() {
                    request.args = args.clone();
                }
                request
                    .validate()
                    .map_err(|error| fail(&error.to_string()))?;
                steps.push(match name {
                    Some(name) => Step::Spawn(name.into(), request),
                    None => Step::Shell(request),
                });
            }
            _ => return Err(fail("unknown zrun instruction")),
        }
    }
    if !pending.is_empty() {
        return Err(invalid(format!("missing await for {}", pending.join(", "))));
    }
    Ok(steps)
}

fn emit(delivery: Delivery) -> io::Result<u8> {
    let result = delivery.result;
    io::stdout().lock().write_all(&result.stdout)?;
    io::stderr().lock().write_all(&result.stderr)?;
    if let Some(error) = &result.error {
        writeln!(io::stderr().lock(), "zrun: {error}")?;
    }
    if result.truncated {
        writeln!(
            io::stderr().lock(),
            "zrun: captured output exceeded its limit"
        )?;
    }
    Ok(crate::exit_code(result.exit_code))
}

pub(crate) fn run(shell: PathBuf, source: &str, args: Vec<String>) -> io::Result<u8> {
    let steps = parse_with_args(source, Arc::new(args))?;
    if let Some(path) = std::env::var_os("ZRUN_SOCKET") {
        let path = std::sync::Arc::new(PathBuf::from(path));
        let pool = Arc::new(crate::mcp::RemotePool::default());
        let cwd = std::env::current_dir()?;
        let mut runtime = crate::executor::Executor::current()?;
        let spawner = runtime.spawner();
        let io = runtime.io().clone();
        let timer = runtime.timer();
        runtime.block_on(execute(steps, async |requests: Vec<Request>| {
            requests
                .into_iter()
                .map(|mut request| {
                    request.cwd = Some(
                        request
                            .cwd
                            .as_deref()
                            .map_or_else(|| cwd.clone(), |dir| cwd.join(dir)),
                    );
                    let (sender, receiver) = zrun::sync::channel();
                    let (io, path, timer) = (io.clone(), path.clone(), timer.clone());
                    let pool = Arc::clone(&pool);
                    spawner
                        .spawn(async move {
                            let delivery = pool
                                .run_one(&io, &path, request, &timer)
                                .await
                                .unwrap_or_else(Delivery::rejected);
                            let _ = sender.send(delivery);
                        })
                        .map_err(io::Error::other)?;
                    Ok(Ticket::Pending(receiver))
                })
                .collect()
        }))?
    } else {
        Batch::new(shell)?.drive(async move |broker| {
            execute(steps, async |requests| broker.submit_batch(requests).await).await
        })
    }
}

async fn execute(
    steps: Vec<Step>,
    mut submit: impl AsyncFnMut(Vec<Request>) -> Vec<io::Result<Ticket>>,
) -> io::Result<u8> {
    let mut pending = std::collections::BTreeMap::new();
    let mut status = 0;
    let mut steps = steps.into_iter().peekable();
    while let Some(step) = steps.next() {
        let delivery = match step {
            Step::Spawn(name, request) => {
                let mut names = vec![name];
                let mut requests = vec![request];
                while let Some(Step::Spawn(name, request)) =
                    steps.next_if(|step| matches!(step, Step::Spawn(..)))
                {
                    names.push(name);
                    requests.push(request);
                }
                let tickets = submit(requests).await;
                pending.extend(names.into_iter().zip(tickets));
                continue;
            }
            Step::Await(name) => pending.remove(&name).expect("validated await"),
            Step::Shell(request) => submit(vec![request])
                .await
                .pop()
                .expect("one ticket for one request"),
        };
        let delivery = match delivery {
            Ok(ticket) => match ticket.await {
                Ok(delivery) => delivery,
                Err(error) => Delivery::rejected(io::Error::other(error)),
            },
            Err(error) => Delivery::rejected(error),
        };
        let code = emit(delivery)?;
        if status == 0 {
            status = code;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> io::Result<Vec<Step>> {
        parse_with_args(source, Arc::default())
    }

    #[test]
    fn validates_named_await_and_keeps_shell_block_syntax() {
        let steps = parse("# zrun:1\nasync job <<END\nfor x in a b; do printf '%s' \"$x\"; done\nEND\nawait job\n").unwrap();
        let Step::Spawn(name, request) = &steps[0] else {
            panic!("expected async")
        };
        assert_eq!(name, "job");
        assert!(request.script.contains("for x in a b; do"));
        assert!(parse("# zrun:1\nawait missing\n").is_err());
        assert!(parse("# zrun:1\nasync job <<END\necho hi\nEND\n").is_err());
    }
    #[test]
    fn rejects_invalid_requests_before_any_task_is_started() {
        let source = concat!(
            "# zrun:1\n",
            "async one {\"script\":\"true\"}\n",
            "await one\n",
            "shell {\"script\":\"true\",\"timeout_ms\":0}\n",
        );
        let Err(error) = parse(source) else {
            panic!("expected invalid timeout")
        };
        assert!(error.to_string().contains("line 4"));
        assert!(parse("# zrun:1\nasync job <<END\nprintf hello\n").is_err());
    }

    #[test]
    fn preserves_native_cache_configuration_and_allows_name_reuse() {
        let source = concat!(
            "# zrun:1\n",
            "async one {\"script\":\"printf a\",\"reuse\":{\"scope\":\"fixture\",\"input_revision\":\"v1\",\"ttl_ms\":5000}}\n",
            "await one\n",
            "async one {\"script\":\"printf a\"}\n",
            "await one\n",
        );
        let steps = parse(source).unwrap();
        let Step::Spawn(_, request) = &steps[0] else {
            panic!("expected async")
        };
        assert_eq!(request.reuse.as_ref().unwrap().input_revision, "v1");
        assert_eq!(steps.len(), 4);
    }
}
