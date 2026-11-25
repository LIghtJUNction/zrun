import hashlib
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release_runtime as release


class RuntimePublicationTests(unittest.TestCase):
    def setUp(self):
        self.sha = "4" * 40
        self.version = "0.1.0-p0.13"
        self.checksums = {name: hashlib.sha256(name.encode()).hexdigest() for name in release.PACKAGES}
        self.receipt = {"sha": self.sha, "version": self.version, "packages": self.checksums}

    def test_partial_retry_keeps_only_missing_packages_in_dependency_order(self):
        existing = set(release.PACKAGES[:4])

        def read(name, version):
            self.assertEqual(version, self.version)
            return {"cksum": self.checksums[name], "yanked": False} if name in existing else None

        self.assertEqual(release.missing_packages(self.receipt, read), ["zrun-executor", "zrun"])

    def test_yanked_or_different_existing_package_aborts_the_batch(self):
        for yanked, checksum in [(True, self.checksums["zrun"]), (False, "0" * 64)]:
            with self.subTest(yanked=yanked), self.assertRaisesRegex(ValueError, "yanked or has different package bytes"):
                release.missing_packages(
                    self.receipt, lambda name, _: {"cksum": checksum, "yanked": yanked} if name == "zrun" else None
                )

    def test_different_commit_in_archive_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary, patch("release_runtime.subprocess.run"):
            with patch.object(release, "Path", side_effect=lambda path: Path(temporary) / path):
                directory = Path(temporary) / "target/package"
                directory.mkdir(parents=True)
                prefix = f"zrun-{self.version}"
                with tarfile.open(directory / (prefix + ".crate"), "w:gz") as archive:
                    files = {
                        ".cargo_vcs_info.json": json.dumps({"git": {"sha1": "a" * 40}}),
                        "Cargo.toml": f'[package]\nname="zrun"\nversion="{self.version}"\n',
                    }
                    for name, text in files.items():
                        data = text.encode()
                        member = tarfile.TarInfo(f"{prefix}/{name}")
                        member.size = len(data)
                        archive.addfile(member, io.BytesIO(data))
                with self.assertRaisesRegex(ValueError, "source or package identity"):
                    release.package_bytes(["zrun"], self.sha, self.version, verify=False)

    def test_resume_rejects_repacked_bytes_that_differ(self):
        manifest = io.BytesIO(f'[package]\nversion = "{self.version}"\n'.encode())
        with (
            patch.dict(os.environ, SOURCE_SHA=self.sha, VERIFIED_RECEIPT=json.dumps(self.receipt)),
            patch("release_runtime.subprocess.check_output", return_value=self.sha),
            patch("builtins.open", return_value=manifest),
            patch("release_runtime.missing_packages", return_value=["zrun"]),
            patch("release_runtime.package_bytes", return_value={"zrun": "0" * 64}),
            self.assertRaisesRegex(ValueError, "repacked bytes differ"),
        ):
            release.main("resume")


if __name__ == "__main__":
    unittest.main()
