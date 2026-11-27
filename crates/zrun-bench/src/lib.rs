//! Measurements shared by the standalone comparator processes.

/// Linux process-lifetime high-water RSS at the end of the workload, in bytes.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| {
                let mut fields = line.strip_prefix("VmHWM:")?.split_whitespace();
                let kib = fields.next()?.parse::<u64>().ok()?;
                (fields.next()? == "kB").then_some(kib)?.checked_mul(1024)
            })
    }
    #[cfg(not(target_os = "linux"))]
    None
}
