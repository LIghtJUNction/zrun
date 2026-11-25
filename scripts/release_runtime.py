"""Verify and resume publication of the six public zrun runtime crates."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import time
import tomllib
from urllib.error import HTTPError
from urllib.request import urlopen

PACKAGES = ("zrun-io", "zrun-sync", "zrun-task", "zrun-time", "zrun-executor", "zrun")


def registry_entry(name, version):
    url = f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}"
    try:
        with urlopen(url, timeout=20) as response:
            return next((row for row in map(json.loads, response) if row["vers"] == version), None)
    except HTTPError as error:
        if error.code != 404:
            raise
        return None


def missing_packages(receipt, read=registry_entry):
    missing = []
    for name in PACKAGES:
        entry = read(name, receipt["version"])
        if entry is None:
            missing.append(name)
        elif entry["yanked"] or entry["cksum"] != receipt["packages"][name]:
            raise ValueError(f"{name} {receipt['version']} is yanked or has different package bytes")
    return missing


def package_bytes(names, sha, version, verify):
    args = ["cargo", "package", "--locked", "--target-dir", "target"]
    if not verify:
        args.append("--no-verify")
    for name in names:
        args.extend(("-p", name))
    subprocess.run(args, check=True)
    checksums = {}
    for name in names:
        prefix = f"{name}-{version}"
        path = Path("target/package") / (prefix + ".crate")
        with tarfile.open(path) as archive:
            vcs = json.load(archive.extractfile(f"{prefix}/.cargo_vcs_info.json"))
            package = tomllib.loads(archive.extractfile(f"{prefix}/Cargo.toml").read().decode())["package"]
        if vcs["git"] != {"sha1": sha} or (package["name"], package["version"]) != (name, version):
            raise ValueError(f"source or package identity differs in {path}")
        checksums[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    return checksums


def main(mode):
    sha = os.environ["SOURCE_SHA"]
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if sha != head or not re.fullmatch("[0-9a-f]{40}", sha):
        raise ValueError("checkout differs from the resolved commit")
    with open("Cargo.toml", "rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    if not re.fullmatch(r"0\.1\.0-p0\.[1-9][0-9]*", version):
        raise ValueError("expected a P0 preview version")
    if mode == "prepare":
        checksums = package_bytes(PACKAGES, sha, version, verify=True)
        receipt = {"version": version, "sha": sha, "packages": checksums}
    else:
        receipt = json.loads(os.environ["VERIFIED_RECEIPT"])
        if receipt["version"] != version or receipt["sha"] != sha or set(receipt["packages"]) != set(PACKAGES):
            raise ValueError("receipt differs from the selected source or runtime package set")

    missing = missing_packages(receipt)
    if mode == "confirm":
        deadline = time.monotonic() + 180
        while missing and time.monotonic() < deadline:
            time.sleep(5)
            missing = missing_packages(receipt)
        if missing:
            raise ValueError(f"registry visibility timed out for {', '.join(missing)}")
    elif mode == "resume" and missing:
        checksums = package_bytes(missing, sha, version, verify=False)
        if any(receipt["packages"][name] != checksum for name, checksum in checksums.items()):
            raise ValueError("repacked bytes differ from the verified release")

    print(f"{sha} {version}: {len(PACKAGES) - len(missing)} already published, pending: {', '.join(missing) or 'none'}")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            print("missing=" + " ".join(missing), file=stream)
            if mode == "prepare":
                print("receipt=" + json.dumps(receipt, separators=(",", ":")), file=stream)


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in {"prepare", "resume", "confirm"}:
        sys.exit("usage: release_runtime.py {prepare|resume|confirm}")
    main(sys.argv[1])
