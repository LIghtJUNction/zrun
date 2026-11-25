use super::{
    error,
    execute::{self, Control, State, Streams},
    syntax::Program,
};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    os::fd::OwnedFd,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

struct Output {
    code: u8,
    out: Vec<u8>,
    err: Vec<u8>,
    truncated: bool,
}
struct Task {
    name: String,
    control: Arc<Control>,
    worker: thread::JoinHandle<()>,
    result: Option<io::Result<Output>>,
}
pub(super) struct Tasks {
    tasks: VecDeque<Task>,
    sender: mpsc::Sender<(String, io::Result<Output>)>,
    receiver: mpsc::Receiver<(String, io::Result<Output>)>,
}
fn capture(mut input: io::PipeReader) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let take = n.min(crate::execution::MAX_OUTPUT - output.len());
        output.extend_from_slice(&buffer[..take]);
        truncated |= take < n;
    }
    Ok((output, truncated))
}
fn collect(mut state: State, program: Program, control: Arc<Control>) -> io::Result<Output> {
    let (or, ow) = io::pipe()?;
    let (er, ew) = io::pipe()?;
    let streams = Streams {
        input: File::open("/dev/null")?,
        out: OwnedFd::from(ow).into(),
        err: OwnedFd::from(ew).into(),
    };
    thread::scope(|scope| {
        let out = scope.spawn(|| capture(or));
        let err = scope.spawn(|| capture(er));
        let code = execute::execute(&mut state, &program, &streams, &control, false);
        drop(streams);
        let (out, a) = out.join().map_err(|_| error("stdout collector failed"))??;
        let (err, b) = err.join().map_err(|_| error("stderr collector failed"))??;
        Ok(Output {
            code,
            out,
            err,
            truncated: a || b,
        })
    })
}
impl Tasks {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            tasks: VecDeque::new(),
            sender,
            receiver,
        }
    }
    pub fn start(&mut self, name: &str, state: &State, program: Program) -> io::Result<()> {
        if self.tasks.len() >= 128 || self.tasks.iter().any(|t| t.name == name) {
            return Err(error("duplicate name or 128 pending tasks"));
        }
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err(error("invalid task name"));
        }
        let name = name.to_owned();
        let control = Arc::new(Control::default());
        let token = control.clone();
        let owner = state.clone();
        let sender = self.sender.clone();
        let tag = name.clone();
        let worker = thread::Builder::new()
            .name("zrun-task".into())
            .spawn(move || {
                let result = collect(owner, program, token);
                let _ = sender.send((tag, result));
            })?;
        self.tasks.push_back(Task {
            name,
            control,
            worker,
            result: None,
        });
        Ok(())
    }
    pub fn wait(
        &mut self,
        spec: &str,
        mut interrupted: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<u8> {
        let ready = spec == "--ready";
        let mut names = if ready || spec == "*" {
            self.tasks
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
        } else {
            let mut ns = Vec::new();
            for n in spec.split_whitespace() {
                if ns.iter().any(|k| k == n) || !self.tasks.iter().any(|t| t.name == n) {
                    return Err(error(format!("not a pending task: {n}")));
                }
                ns.push(n.to_owned());
            }
            if ns.is_empty() {
                return Err(error("usage: await NAME... | * | --ready"));
            }
            ns
        };
        let mut status = 0;
        while !names.is_empty() {
            if interrupted()? {
                return Ok(130);
            }
            let choice = if ready {
                names.iter().position(|n| {
                    self.tasks
                        .iter()
                        .any(|t| &t.name == n && t.result.is_some())
                })
            } else {
                self.tasks
                    .iter()
                    .any(|t| t.name == names[0] && t.result.is_some())
                    .then_some(0)
            };
            if let Some(index) = choice {
                let name = names.remove(index);
                let k = self.tasks.iter().position(|t| t.name == name).unwrap();
                let t = self.tasks.remove(k).unwrap();
                let _ = t.worker.join();
                let r = t.result.unwrap()?;
                io::stdout().lock().write_all(&r.out)?;
                io::stdout().lock().flush()?;
                io::stderr().lock().write_all(&r.err)?;
                if r.truncated {
                    eprintln!("zrun: {name}: captured output exceeded its limit");
                }
                if status == 0 {
                    status = r.code;
                }
            } else {
                match self.receiver.recv_timeout(Duration::from_millis(40)) {
                    Ok((name, result)) => {
                        if let Some(t) = self.tasks.iter_mut().find(|t| t.name == name) {
                            t.result = Some(result);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return Err(error("task channel closed")),
                }
            }
        }
        Ok(status)
    }
}
impl Drop for Tasks {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.control.cancel();
        }
        for t in self.tasks.drain(..) {
            let _ = t.worker.join();
        }
    }
}
