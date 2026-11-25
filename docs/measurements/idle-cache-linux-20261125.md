# Idle broker cache reclamation — Linux cloud, 2026-11-25

Base: `4bd2913188ff665e78a2f3b16f776cc8be764f51` (P0.13), plus this patch.
An expired reuse entry stopped serving hits but retained its key and captured
stdout/stderr indefinitely until replacement/eviction or runtime shutdown.
This was bounded by 1,024 entries / 16 MiB of accounted cache storage; it is
not evidence of an unbounded leak or the cause of a reported OOM.

The broker now schedules one maintenance task that sweeps expired entries once
per second while the runtime is driven. It releases raw results and both index
references, updates byte accounting, and retains live entries. Runtime shutdown
owns this task like the existing workers. Current-thread stdio pauses its
runtime while blocking on stdin, so idle stdio is not an autonomous sweep gate.
Command execution, TTL validity, ordering and wire formats are unchanged.

## Reproduction and limits

Run `python3 scripts/cache-memory.py /absolute/path/to/zrun` for each binary,
sequentially. Three independent daemon processes per revision; debug builds,
Cargo `--locked`, Rust 1.100.0 (`0ef1bc386`), x86_64 Linux 6.18.57,
AMD EPYC 9V74 (5 available vCPUs), approximately 18 GiB RAM, no swap.
Build batches used the cloud-local `/workspace/cargo-target` and its outer
`.project-z-cargo-owner.lock`; no user-machine target/cache was accessed.

Each process uses one runtime worker, one outstanding Unix HTTP `shell_run`
request, 120 unique reuse keys, 10 s TTL, exactly 65,536 bytes on each stream,
then 11.2 s idle. Synthetic child diagnostics record their own `ru_maxrss` in
the temporary directory. The probe checks byte-exact output, exit 0, no
truncation, a live cache hit and re-execution after expiry. Temporary files and
the daemon are cleaned on exit. Each daemon/child inherits a 512 MiB address
space ceiling and disabled core dumps. A watchdog terminates owned descendants
and the daemon above 192 MiB daemon RSS, 128 MiB sampled descendant RSS, or
30 s per repetition; socket operations also have deadlines.

## Measurements (KiB)

| Metric | Before: three samples | After: three samples |
| --- | --- | --- |
| Daemon peak RSS / VmHWM | 20592, 20488, 20552 | 20500, 20596, 20596 |
| Daemon idle RSS | 20492, 20488, 20552 | 20500, 20340, 20540 |
| Command child self-reported peak RSS | 6528, 6528, 6528 | 6528, 6528, 6528 |

Mean peak RSS: before 20544 (95% Student-t CI 20414–20674), after 20564
(20426–20702). Mean idle RSS: before 20511 (20422–20600), after 20460
(20197–20723). With only three runs these are descriptive estimates; there is
no demonstrated RSS improvement. Allocation release does not require the
allocator to return its high-water pages to the OS. The new weak-reference
regressions establish that expired result allocations are no longer owned by
the cache, including an idle broker with no subsequent submission.

The `/proc` ancestry sampler missed short-lived descendants in these final
runs (zero sampled); it does not establish zero descendant usage or a complete
process-tree peak. The table separately reports each synthetic command child's
self-measured peak. No OOM was triggered and no exhaustion root cause is claimed.
Raw JSONL samples are outside Git at `/workspace/cache-before-final.jsonl` and
`/workspace/cache-after-final.jsonl` in this cloud workspace.

## Verification and remaining boundaries

`cargo test --locked -p zrun-cli`: 81 passed, including two new reclamation
regressions and existing CLI, Unix/TCP, binary output, timeout, coalescing,
compact-output and PTY tests. `cargo fmt --all -- --check` and
`cargo clippy --locked -p zrun-cli --all-targets -- -D warnings` passed.
The broader `cargo test --locked --features process` passed the process reaper,
I/O, pool and runtime groups (including cancellation/timer storms; the explicit
million-task test stayed ignored). A later PTY test timed out once waiting for
its initial prompt; its isolated rerun passed. The earlier full CLI run passed.

Static boundary review found existing limits of 4,096 waiters, 16 MiB pending
request bytes, 32 executing commands, 64 KiB per captured stream, 128 MiB raw
response reservations, 32 MiB encoded output reservations, 32 MiB HTTP ingress
buffers, and 64 compact output handles per endpoint. These are separate budgets,
not one total-process RSS cap. Compact output TTL cleanup is access-driven;
this patch does not change it. Dropped tickets can retain bounded subscriptions
until command completion/timeout. Sustained overload, large deduplication and
disconnect storms, 24-hour soak, allocator behavior across platforms and real
consumer workloads remain separate unverified gates. No deployment or release.
