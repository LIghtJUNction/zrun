use zrun_plugin_api::{ALLOW, Admission, AdmissionExport, DENY};

struct Restrictive;

impl Admission for Restrictive {
    fn command(&mut self, _: &str, _: &str, script: &str, _: u64) -> zplugin::Result<u32> {
        Ok(if script == "exit 63" { DENY } else { ALLOW })
    }
}

fn create() -> zplugin::Result<Restrictive> {
    Ok(Restrictive)
}

zplugin::export!(
    AdmissionExport<Restrictive>,
    constructor = create,
    version = (1, 0, 0),
    unload = pinned
);
