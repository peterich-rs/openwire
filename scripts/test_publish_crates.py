import json
import sys
import tempfile
import unittest
from io import BytesIO
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError, URLError

import publish_crates as release


CORE = "openwire-core"
TOKIO = "openwire-tokio"
RUSTLS = "openwire-rustls"
SELECTION = {CORE: "0.1.3", TOKIO: "0.1.3"}


class ReleasePlanTests(unittest.TestCase):
    def read_plan(self, contents, version="0.1.3"):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "release.json"
            path.write_text(contents, encoding="utf-8")
            return release.read_release(path, version, {**SELECTION, RUSTLS: "0.1.2"})

    def test_manifest_selects_a_subset_with_exact_versions(self):
        selection = {CORE: "0.1.3", RUSTLS: "0.1.2"}
        result = self.read_plan(json.dumps({"version": "0.1.3", "crates": selection}))
        self.assertEqual(result, ("0.1.3", selection))

    def test_wrong_tag_or_selection_fails(self):
        with self.assertRaisesRegex(release.ReleaseError, "has no plan"):
            self.read_plan(json.dumps({"version": "0.1.3", "crates": SELECTION}), "0.1.2")
        for selection, message in [
            ({CORE: "0.1.2"}, "workspace has 0.1.3"),
            ({"openwire-test": "0.1.2"}, "not a crates.io publishable"),
            ({}, "select at least one"),
        ]:
            with self.subTest(selection=selection), self.assertRaisesRegex(release.ReleaseError, message):
                self.read_plan(json.dumps({"version": "0.1.3", "crates": selection}))

    def test_duplicate_entries_fail(self):
        with self.assertRaisesRegex(release.ReleaseError, "duplicate release plan field"):
            self.read_plan('{"version":"0.1.3","crates":{"openwire-core":"0.1.2","openwire-core":"0.1.3"}}')


class RegistryTests(unittest.TestCase):
    @patch.object(release, "urlopen")
    def test_index_requires_an_exact_version_and_rejects_yanks(self, open_url):
        def response(yanked):
            return BytesIO((json.dumps({"name": CORE, "vers": "0.1.3", "yanked": yanked}) + "\n").encode())

        open_url.side_effect = [response(False), response(False), response(True), response(False)]
        self.assertFalse(release.registry_version_exists(CORE, "0.1.2"))
        self.assertTrue(release.registry_version_exists(CORE, "0.1.3"))
        self.assertEqual(open_url.call_args.args[0].full_url, "https://index.crates.io/op/en/openwire-core")
        with self.assertRaisesRegex(release.ReleaseError, "is yanked"):
            release.registry_version_exists(CORE, "0.1.3")
        with self.assertRaisesRegex(release.ReleaseError, "occupies"):
            release.registry_version_exists(CORE, "0.1.3+local")

    @patch.object(release, "urlopen")
    def test_missing_and_unavailable_registry_have_different_outcomes(self, open_url):
        for status in (404, 410, 429, 503):
            open_url.side_effect = HTTPError("https://index.crates.io", status, "", {}, None)
            if status in (404, 410):
                self.assertFalse(release.registry_version_exists(CORE, "0.1.3"))
            else:
                with self.assertRaisesRegex(release.ReleaseError, f"HTTP {status}"):
                    release.registry_version_exists(CORE, "0.1.3")
        open_url.side_effect = URLError("offline")
        with self.assertRaisesRegex(release.ReleaseError, "offline"):
            release.registry_version_exists(CORE, "0.1.3")

    @patch.object(release, "urlopen", return_value=BytesIO(b'{"name":"openwire-core"}\n'))
    def test_malformed_index_record_fails(self, _open_url):
        with self.assertRaisesRegex(release.ReleaseError, "invalid crates.io index entry"):
            release.registry_version_exists(CORE, "0.1.3")


class ReleaseCommandTests(unittest.TestCase):
    @patch.object(release, "read_release", return_value=("0.1.3", SELECTION))
    @patch.object(release, "publishable_packages", return_value=SELECTION)
    @patch.object(release, "registry_version_exists")
    @patch.object(release.subprocess, "run")
    def test_ci_packages_selected_crates_without_uploading(self, run, lookup, _packages, _plan):
        with patch.object(sys, "argv", ["publish_crates.py", "--dry-run"]):
            release.main()
        lookup.assert_not_called()
        run.assert_called_once_with(
            ["cargo", "package", "--locked", "--registry", "crates-io", "--all-features",
             "-p", f"{CORE}@0.1.3", "-p", f"{TOKIO}@0.1.3"],
            cwd=release.ROOT,
            check=True,
        )

    @patch.object(release, "read_release", return_value=("0.1.3", SELECTION))
    @patch.object(release, "publishable_packages", return_value=SELECTION)
    @patch.object(release, "registry_version_exists", side_effect=[True, False, False, True])
    @patch.object(release.time, "sleep")
    @patch.object(release.subprocess, "run")
    def test_retry_uploads_only_missing_versions_and_verifies_visibility(self, run, sleep, lookup, _packages, _plan):
        with patch.object(sys, "argv", ["publish_crates.py", "--publish", "--release", "0.1.3"]):
            release.main()
        run.assert_called_once_with(
            ["cargo", "publish", "--locked", "--registry", "crates-io", "-p", f"{TOKIO}@0.1.3"],
            cwd=release.ROOT,
            check=True,
        )
        self.assertEqual(lookup.call_args_list[-1].args, (TOKIO, "0.1.3"))
        sleep.assert_called_once_with(5)

    @patch.object(release, "read_release", return_value=("0.1.3", SELECTION))
    @patch.object(release, "publishable_packages", return_value=SELECTION)
    @patch.object(release, "registry_version_exists", return_value=True)
    @patch.object(release.subprocess, "run")
    def test_completed_release_is_a_no_op(self, run, _lookup, _packages, _plan):
        with patch.object(sys, "argv", ["publish_crates.py", "--publish", "--release", "0.1.3"]):
            release.main()
        run.assert_not_called()
