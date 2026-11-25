# Linux command-tree memory boundaries

Status on 2026-11-25: capability audit and a blocked kernel acceptance gate.
This document adds no runtime policy. No isolated-task OOM test passed in the
Codex cloud environment described below.

## Existing owners and the remaining gap

Audit source: zrun `4bd2913188ff665e78a2f3b16f776cc8be764f51`,
[SSMCP `d2c6d867`](https://github.com/LIghtJUNction/ssmcp/blob/d2c6d867b61de958900443cccc152531616223a0/src/backend/command_memory.rs),
[zmem `18bc12b2`](https://github.com/LIghtJUNction/zmem/blob/18bc12b2d7f11dfd5e30f84e33652b6786ceb25c/src/linux.rs).

| Entry point | Existing boundary | Consequence |
| --- | --- | --- |
| zrun run / batch / MCP | Shared request/output budgets, bounded command count, timeout and process-group cleanup | No independent memory.max for each command tree, and no native OOM-cause field |
| zrun --zmem CLASS ... | Starts the entire zrun instance through zmem; descendants inherit the scope | A whole-instance boundary cannot prove daemon or unrelated-task survival when that group is killed |
| zmem interactive / background | Protection / background policy; no hard MemoryMax set by these launch branches | Membership or MemoryHigh alone is not a task hard limit |
| zmem disposable --max-memory MiB | Existing MemoryMax, MemoryHigh, MemorySwapMax equal to the supplied maximum; OOMPolicy=kill | Swap is permitted up to that separate bound; this is not a swap=0 contract |
| SSMCP Linux command execution | Existing shared executor ceiling plus command child groups; default min(half physical RAM, 8 GiB), swap=0, command memory.oom.group=1 | Reuse this provider/ownership contract rather than copying another cgroup manager into zrun |

SSMCP attaches the child to its group before exec and fails startup if neither
a suitable inherited limit nor delegation can enforce its policy. Its inherited
boundary fast path does not create a new independent group per command. That
fast path therefore does not itself establish isolated-task survival semantics.
The production shared executor and child groups use the **same** default cap;
a synthetic lower child cap does not prove that siblings survive exhaustion of
the production shared parent. The executor group has memory.oom.group=0, while
new command child groups have memory.oom.group=1. A shared-parent OOM can choose
victims across the concurrent command groups, so independence is not guaranteed
by those defaults.

SSMCP's embedded zrun process/library path and its independent output store are
separate from the zrun CLI/MCP broker. CLI request/output reservation results
must not be relabelled as SSMCP output-store enforcement or SSMCP end-to-end
memory acceptance.
SSMCP's existing `memory_ceiling_applies_to_the_command_and_its_children` test
checks a 64 MiB group, child allocation, group kill and group cleanup. It returns
early for an inherited-boundary-only runner, so an overall green test run is not
proof that this exact delegated case executed. The existing test also does not
cover a zrun daemon and an unrelated concurrent task surviving.

## Environment gate — before any allocation

Run `python3 scripts/memory-delegation.py`. Exit 77 means blocked; exit 0 means
only that a route merits a controlled probe, never that enforcement passed.
The script reads the existing namespace and queries the existing user manager.
It does not create a manager, change a host unit, enable controllers or request
privileges. A writable-looking path or a successful systemd request still needs
kernel verification inside the actual task's namespace before running a load.

Observed cloud blocker: `/proc/self/cgroup` was `0::/`; `/sys/fs/cgroup` was
cgroup v2 mounted read-only, not an owned writable subtree; `/run/user/1000` was
absent, and `systemctl --user show-environment` could not connect to a user bus.
The probe reports `blocked`, `enforcement_tested=false`, exit 77. No synthetic
OOM allocation was launched, and no privilege escalation or host setup was tried.

The existing SSMCP CI has broad Linux integration jobs with privileged service
setup; it exposes no narrowly selectable, already-delegated zrun task-memory
job. No new CI dispatch or privileged setup was performed for this documentation
change. Local delegation cannot be inferred from another machine's historical CI.

## Small experiment on an already-authorized delegated runner

Keep the daemon outside the limited task group. Use the existing provider, or
an existing systemd **user transient service per command**, with these explicit
test settings: MemoryMax=64M, MemorySwapMax=0, OOMPolicy=kill,
KillMode=control-group, TasksMax=8, RuntimeMaxSec=10s. The supervisor/client stays
outside; all payload descendants enter before exec/allocation. An ancestor may
impose a tighter limit, so record ancestors too. Never move the daemon into the
64 MiB test unit or enable a host controller as part of the test.

For a direct delegated implementation, the corresponding controls are
memory.max=67108864, memory.swap.max=0 and memory.oom.group=1, with a populated
leaf beneath the delegated parent. For a user service, inspect actual controls
instead of assuming the requested properties were installed. These are kernel
group limits, unlike per-process RLIMIT or a sampled RSS watchdog.
[Kernel memory controls](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)
and [systemd resource controls](https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html)
define the distinction; memory.high throttles and is not this hard-limit gate.

Required acceptance, still unexecuted here:

1. Through the actual Unix MCP shell_batch entry, concurrently submit a bounded
   normal task and a distinct limited task. Save the daemon PID/start identity.
   Before allocating, the limited payload reads back membership, all three
   controls, ancestor limits and initial memory.events, memory.events.local and
   memory.peak where available. Reject missing/mismatched
   controls. Keep evidence outside the soon-to-be-killed group.
2. In the limited leaf only, fork exactly two children that each attempt and
   touch at most 40 MiB. This bounded aggregate exceeds 64 MiB while keeping the
   requested allocation small. Do not use a real build or allocate until host
   exhaustion. Record OOM event deltas or systemd Result=oom-kill; a nonzero exit
   or SIGKILL alone cannot distinguish memory OOM from cancellation/timeout.
3. Verify the limited tree is terminated, its error is identifiable from that
   evidence, and the unrelated task returns its expected bytes and exit status.
   Issue a third health request and confirm the same daemon is alive. Native
   zrun currently has only exit/error output, so do not claim native typed OOM
   reporting merely because a test wrapper can read the manager's Result.
4. On success, timeout, cancellation and assertion failure, stop only the unique
   test unit / kill only the owned delegated subtree. Wait for cgroup.events
   populated=0 and all recorded process identities to disappear; remove groups,
   transient units and temporary files. A systemd service is not owned by the
   POSIX process group of its client: killing that client alone is insufficient.
5. Read evidence before resetting/collecting a failed unit or removing its
   cgroup. Repeat below-limit and over-limit cases, and record unavailable
   counters explicitly. Ensure the test's own runtime deadline independently
   bounds a failed OOM experiment.

Run three separately labelled modes, not just the lower-child example:

| Mode | Bounded example | What it can establish |
| --- | --- | --- |
| Lower child cap | Shared parent 192 MiB; limited child 64 MiB; bounded sibling; two 40 MiB allocation attempts in the limited leaf | Per-task OOM, sibling and daemon survival only for this deliberately lower-child configuration |
| Shared parent cap | Parent 128 MiB, two children each capped 128 MiB; each attempts at most 80 MiB | Individually below child ceilings but aggregate above parent; attribute the event to the parent/local counters and do not require sibling survival |
| Inherited-only | Existing bounded zero-swap scope; provider returns the inherited-boundary path | Shared inherited protection only; no isolated-task claim and no silent "passed" for the independent-group gate |

Keep an additional bounded outer test unit separate from the daemon, account
for interpreter/runtime overhead, and verify each example's real ancestor
limits before starting. Sample local events and peaks at parent and children
before cleanup; mark unavailable counters explicitly. Do not sum hierarchical
memory.events and mistake a descendant event for a parent-local OOM. These
three cases are unexecuted requirements here, not results of the cloud audit.

Only after this gate executes should a production adapter or typed OOM result
be proposed. Reusing SSMCP/zmem must preserve source ownership, avoid a dependency
cycle, and keep any new command policy explicit. No default policy is selected
by this audit. Project Z still requires coherent refs, check, resolve --all and
affected behavior tests; kernel isolation evidence does not relax those gates.
