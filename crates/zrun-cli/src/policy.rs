use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Instant;

use serde::Deserialize;

use crate::execution::Request;

const TOOL: &str = "shell.run";
const ALLOW: [zguard::Rule<'static>; 1] = [zguard::Rule::new(
    1,
    zguard::ActionKind::Command,
    TOOL,
    zguard::Decision::Allow,
)];
const DENY: [zguard::Rule<'static>; 1] = [zguard::Rule::new(
    2,
    zguard::ActionKind::Command,
    TOOL,
    zguard::Decision::Deny,
)];
const APPROVAL: [zguard::Rule<'static>; 1] = [zguard::Rule::new(
    3,
    zguard::ActionKind::Command,
    TOOL,
    zguard::Decision::RequireApproval,
)];

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Admission {
    #[default]
    Allow,
    Deny,
    RequireApproval,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    shell: Admission,
    max_calls: u64,
    max_payload_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shell: Admission::Allow,
            max_calls: u64::MAX,
            max_payload_bytes: 72 * 1024,
        }
    }
}

pub(crate) struct Gate {
    guard: zguard::Guard<'static>,
    started: Instant,
    next_call: u64,
}

impl Gate {
    pub(crate) fn from_env() -> io::Result<Self> {
        let settings = match std::env::var_os("ZRUN_POLICY") {
            Some(path) => {
                let mut bytes = Vec::new();
                File::open(path)?.take(16_385).read_to_end(&mut bytes)?;
                if bytes.len() > 16_384 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "zrun policy exceeds 16 KiB",
                    ));
                }
                serde_json::from_slice::<Settings>(&bytes)?
            }
            None => Settings::default(),
        };
        let rules = match settings.shell {
            Admission::Allow => &ALLOW,
            Admission::Deny => &DENY,
            Admission::RequireApproval => &APPROVAL,
        };
        let policy =
            zguard::Policy::new(u64::from(rules[0].id()), rules).map_err(io::Error::other)?;
        let guard = zguard::Guard::new(
            policy,
            zguard::Context {
                session: u64::from(std::process::id()),
                capabilities: 0,
            },
            zguard::Limits {
                max_calls: settings.max_calls,
                max_payload_bytes: settings.max_payload_bytes,
            },
        );
        Ok(Self {
            guard,
            started: Instant::now(),
            next_call: 0,
        })
    }

    pub(crate) fn authorize(&mut self, shell: &Path, request: &Request) -> io::Result<()> {
        self.next_call = self
            .next_call
            .checked_add(1)
            .ok_or_else(|| io::Error::other("command IDs exhausted"))?;
        let call_id = self.next_call.to_string();
        let program = shell.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "shell path must be UTF-8")
        })?;
        let cwd = request
            .cwd
            .as_deref()
            .map(|cwd| {
                cwd.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "command directory must be UTF-8",
                    )
                })
            })
            .transpose()?;
        let args = [program, "-c", &request.script];
        let input = zguard::Request::command(&call_id, TOOL, &args, cwd);
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let decision = self.guard.authorize(input, now, None, &mut zguard::NoAudit);
        if !decision.is_allowed() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("zguard denied the request ({:?})", decision.reason),
            ));
        }
        Ok(())
    }
}
