#!/usr/bin/env python3
"""Read-only Linux memory-boundary preflight. Exit 77 means no usable route.

Eligibility is not proof that a controller or an OOM boundary works. This script
does not create groups/units, move processes, enable controllers, or allocate a
test workload. Positive eligibility still requires kernel readback and the
acceptance protocol in docs/task-memory-boundaries.md.
"""
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def main():
    report = {"schema": "zrun.memory-delegation.v1", "enforcement_tested": False}
    if platform.system() != "Linux":
        report.update(status="blocked", reasons=["Linux cgroup v2 is required"])
        print(json.dumps(report, indent=2))
        return 77
    reasons = []
    membership = read("/proc/self/cgroup")
    unified = [line[3:] for line in (membership or "").splitlines() if line.startswith("0::")]
    group = None
    if len(unified) == 1 and unified[0].startswith("/") and ".." not in Path(unified[0]).parts:
        group = Path("/sys/fs/cgroup") / unified[0].lstrip("/")
    report["membership"] = membership
    report["group"] = str(group) if group else None
    report["controls"] = {
        name: read(group / name) if group else None
        for name in ("cgroup.controllers", "cgroup.subtree_control", "memory.max",
                     "memory.swap.max", "memory.oom.group")
    }
    direct = False
    unified_visible = bool(group and (group / "cgroup.controllers").exists())
    if unified_visible:
        # A writable directory is only a candidate, not a validated delegation.
        enabled = (read(group / "cgroup.subtree_control") or "").split()
        try:
            direct = (group.stat().st_uid == os.geteuid() and os.access(group, os.W_OK)
                      and os.access(group / "cgroup.procs", os.W_OK) and "memory" in enabled)
        except OSError:
            direct = False
    report["direct_subtree_candidate"] = direct
    if not direct:
        reasons.append("current memory subtree is not an owned, writable, already-enabled candidate")

    manager = False
    executable = shutil.which("systemctl")
    if executable:
        try:
            result = subprocess.run([executable, "--user", "show-environment"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=3, text=True)
            manager = result.returncode == 0
            # Never print the manager environment or arbitrary inherited values.
            report["user_manager_returncode"] = result.returncode
        except (OSError, subprocess.TimeoutExpired):
            report["user_manager_returncode"] = None
    report["user_manager_reachable"] = manager
    if not manager:
        reasons.append("no reachable existing systemd user manager")
    if not unified_visible:
        reasons.append("no readable unified cgroup v2 hierarchy for kernel verification")
    eligible = unified_visible and (direct or manager)
    report.update(status="eligible_for_probe" if eligible else "blocked", reasons=reasons)
    print(json.dumps(report, indent=2))
    return 0 if eligible else 77


if __name__ == "__main__":
    sys.exit(main())
