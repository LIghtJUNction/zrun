#![no_std]

//! Versioned native admission interface for commands submitted to zrun.

/// Allow zrun to continue through its own execution limits.
pub const ALLOW: u32 = 0;
/// Deny access to the execution queue and output cache.
pub const DENY: u32 = 1;

/// An operator-installed extension that can veto access to the command broker.
///
/// The host retains control of the shell, arguments, environment, limits and
/// cache. The extension gets the same inputs for a cached and a live request.
#[zplugin::interface(id = "dev.project-z.zrun.admission", version = 1)]
pub trait Admission {
    /// Return the ALLOW or DENY decision for this invocation.
    ///
    /// # Errors
    ///
    /// A plugin error fails the request without executing the command.
    fn command(
        &mut self,
        shell: &str,
        cwd: &str,
        script: &str,
        timeout_ms: u64,
    ) -> Result<u32, zplugin::Error>;
}
