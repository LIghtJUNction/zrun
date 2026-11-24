use zrun_plugin_api::{ALLOW, AdmissionV2, AdmissionV2Export, DENY};

struct RestrictiveArgs;

impl AdmissionV2 for RestrictiveArgs {
    fn command(&mut self, _: &str, _: &str, _: &str, _: u64, args: &[u8]) -> zplugin::Result<u32> {
        Ok(if args.split(|byte| *byte == 0).any(|arg| arg == b"deny") {
            DENY
        } else {
            ALLOW
        })
    }
}

fn create() -> zplugin::Result<RestrictiveArgs> {
    Ok(RestrictiveArgs)
}

zplugin::export!(
    AdmissionV2Export<RestrictiveArgs>,
    constructor = create,
    version = (2, 0, 0),
    unload = pinned
);
