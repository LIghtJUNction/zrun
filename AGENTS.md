# zrun

Read docs/design.md and docs/plan.md. P0 must run without Tokio in its runtime dependency graph. Tokio is a benchmark comparator only. The user authorized public GitHub source publication and a P0 preview Release on 2026-11-23. Keep preview status explicit until later phase gates pass. Deployment and consumer runtime replacement remain separate work.

Do not start agents. Use the existing worker pool with nonoverlapping file ownership. Preserve running work and shared build caches. All ordinary Cargo batches on this host use /var/tmp/project-z-cargo-target and the outer /var/tmp/project-z-cargo-target/.project-z-cargo-owner.lock; the coordinator owns Cargo execution.

Correctness precedes optimization: no duplicate poll, no lost wake, one terminal completion/drop, explicit abort and shutdown, bounded admitted tasks and bounded timer retention. Every unsafe operation documents pointer ownership and lifetime invariants. Miri and Loom results apply only when actually run. Never turn an unavailable tool into a passed gate.

Keep generated data and binaries outside Git. Report measured hardware, compiler, workload, repetitions and confidence intervals; do not infer a performance win from architecture. P1/P2/P3 and the 24-hour soak remain separate gates until executed.
