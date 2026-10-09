"""Offline checks for tag validation, safe source extraction and draft publishing."""
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import pathlib
import stat
import tempfile
import unittest
import warnings
import zipfile
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("release_assets_under_test", ROOT / "scripts/release-assets.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)
SOURCE_ROOT = "SC2TC-Renamer-1.0.0-source"
COMMIT = "a" * 40
OTHER_COMMIT = "b" * 40
CHANGELOG = """# 變更紀錄

## [1.0.1] - 2026-10-10

### Fixed

- 合成的下一版修正。

## [1.0.0] - 2026-10-05

### Added

- 合成的首版功能。

## [0.9.0] - 2026-09-01

- 合成的前一版內容。
"""


class DraftLookupTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "work").mkdir(parents=True, exist_ok=True)

    def test_create_returns_id_and_uses_json_stdin(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "work") as folder:
            notes = pathlib.Path(folder) / "notes.md"
            notes.write_text("版本來源與授權", encoding="utf-8")
            draft = {"id": 7, "tag_name": "v1.0.2", "draft": True}
            response = mock.Mock(returncode=0, stdout=json.dumps(draft), stderr="")
            with mock.patch.object(RELEASE.subprocess, "run", return_value=response) as api:
                self.assertEqual(RELEASE.GhClient("owner/repo").create("v1.0.2", COMMIT, notes), draft)
                self.assertEqual(json.loads(api.call_args.kwargs["input"])["body"], "版本來源與授權")
                self.assertTrue(json.loads(api.call_args.kwargs["input"])["draft"])
                self.assertEqual(json.loads(api.call_args.kwargs["input"])["name"], "SC2TC-Renamer v1.0.2")

    def test_upload_and_publish_use_release_id_not_tag_lookup(self):
        draft = {"id": 7, "tag_name": "v1.0.2", "draft": True}
        with mock.patch.object(RELEASE, "run", return_value="{}") as api:
            client = RELEASE.GhClient("owner/repo")
            client.upload(draft, pathlib.Path("source.zip"))
            upload_command = api.call_args.args[0]
            self.assertIn("/releases/7/assets?name=source.zip", upload_command[2])
            self.assertNotIn("--clobber", upload_command)
            client.publish(draft)
            self.assertIn("repos/owner/repo/releases/7", api.call_args.args[0])
            self.assertIn("draft=false", api.call_args.args[0])

    def test_delete_asset_uses_numeric_asset_id(self):
        with mock.patch.object(RELEASE, "run", return_value="") as api:
            client = RELEASE.GhClient("owner/repo")
            client.delete_asset({"id": 42, "name": "source.zip"})
            command = api.call_args.args[0]
            self.assertIn("repos/owner/repo/releases/assets/42", command)
            self.assertEqual(command[command.index("--method") + 1], "DELETE")
            for asset in [{"name": "source.zip"}, {"id": "42"}, {"id": True}]:
                with self.subTest(asset=asset), self.assertRaises(ValueError):
                    client.delete_asset(asset)
            api.assert_called_once()

    def test_draft_is_found_in_paginated_release_list_after_tag_endpoint_404(self):
        response = mock.Mock(returncode=1, stderr="gh: Not Found (HTTP 404)", stdout="")
        draft = {"id": 7, "tag_name": "v1.0.1", "draft": True, "prerelease": False}
        pages = [[{"id": 6, "tag_name": "v0.9.0", "draft": False}], [draft]]
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response), \
             mock.patch.object(RELEASE, "run", return_value=json.dumps(pages)) as listing:
            self.assertEqual(RELEASE.GhClient("owner/repo").release("v1.0.1"), draft)
            self.assertIn("--paginate", listing.call_args.args[0])
            self.assertIn("--slurp", listing.call_args.args[0])

    def test_absent_release_and_duplicate_tag_list_are_distinguished(self):
        response = mock.Mock(returncode=1, stderr="gh: Not Found (HTTP 404)", stdout="")
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response), \
             mock.patch.object(RELEASE, "run", return_value="[[]]"):
            self.assertIsNone(RELEASE.GhClient("owner/repo").release("v1.0.1"))
        duplicated = [[{"tag_name": "v1.0.1"}, {"tag_name": "v1.0.1"}]]
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response), \
             mock.patch.object(RELEASE, "run", return_value=json.dumps(duplicated)):
            with self.assertRaises(ValueError):
                RELEASE.GhClient("owner/repo").release("v1.0.1")

    def test_permission_error_is_not_treated_as_an_absent_release(self):
        response = mock.Mock(returncode=1, stderr="gh: Forbidden (HTTP 403)", stdout="")
        with mock.patch.object(RELEASE.subprocess, "run", return_value=response), \
             mock.patch.object(RELEASE, "run") as listing:
            with self.assertRaises(RuntimeError):
                RELEASE.GhClient("owner/repo").release("v1.0.1")
            listing.assert_not_called()


class FakeGhClient:
    def __init__(self, release=None, assets=None, commit=COMMIT):
        self.value = copy.deepcopy(release)
        self.uploaded = copy.deepcopy(assets or [])
        self.commit = commit
        self.events = []
        self.corrupt_upload = False
        self.next_id = 100

    def tag_commit(self, tag):
        self.events.append(("tag_commit", tag))
        return self.commit

    def release(self, tag):
        self.events.append(("release", tag))
        return copy.deepcopy(self.value)

    def release_by_id(self, release_id):
        self.events.append(("release_by_id", release_id))
        return copy.deepcopy(self.value)

    def assets(self, release):
        self.events.append(("assets", release["id"]))
        return copy.deepcopy(self.uploaded)

    def create(self, tag, commit, notes):
        self.events.append(("create", tag, commit, notes))
        self.value = {
            "id": 1, "tag_name": tag, "draft": True, "prerelease": False,
            "html_url": "https://example.invalid/release",
        }
        return copy.deepcopy(self.value)

    def upload(self, release, path):
        tag = release["tag_name"]
        self.events.append(("upload", tag, path.name))
        if any(asset["name"] == path.name for asset in self.uploaded):
            raise AssertionError("The publisher attempted to replace an existing asset.")
        digest = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
        if self.corrupt_upload:
            digest = "sha256:" + "0" * 64
        self.next_id += 1
        self.uploaded.append({
            "id": self.next_id, "name": path.name, "size": path.stat().st_size, "digest": digest,
            "state": "uploaded",
        })

    def delete_asset(self, asset):
        self.events.append(("delete", asset["name"]))
        if not self.value["draft"]:
            raise AssertionError("The publisher attempted to delete an asset of a published release.")
        remaining = [item for item in self.uploaded if item["id"] != asset["id"]]
        if len(remaining) != len(self.uploaded) - 1:
            raise AssertionError("The publisher deleted an unknown asset.")
        self.uploaded = remaining

    def publish(self, release):
        tag = release["tag_name"]
        self.events.append(("publish", tag))
        self.value["draft"] = False

    def writes(self):
        return [event for event in self.events if event[0] in ("create", "upload", "delete", "publish")]


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        work = ROOT / "work/release-tests"
        work.mkdir(parents=True, exist_ok=True)
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="fixture-", dir=work))
        self.asset_root = self.root / "assets"
        self.asset_root.mkdir()
        self.notes = self.root / "notes.md"
        self.notes.write_text("Synthetic release notes.\n", encoding="utf-8")
        self.expected = []
        for name, data in [
            ("portable.zip", b"synthetic executable archive"),
            ("source.zip", b"synthetic matching source archive"),
            ("SHA256SUMS.txt", b"synthetic hashes\n"),
        ]:
            path = self.asset_root / name
            path.write_bytes(data)
            self.expected.append({
                "name": name, "size": len(data),
                "digest": "sha256:" + hashlib.sha256(data).hexdigest(),
            })
        self.plan = {"tag": "v1.0.0", "source_commit": COMMIT, "assets": self.expected}

    def uploaded(self):
        return [dict(asset, state="uploaded", id=index + 1) for index, asset in enumerate(self.expected)]

    def changelog(self, text=CHANGELOG):
        (self.root / "CHANGELOG.md").write_text(text, encoding="utf-8")

    def metadata(self, draft=True):
        return {
            "id": 1, "tag_name": self.plan["tag"], "draft": draft, "prerelease": False,
            "html_url": "https://example.invalid/release",
        }

    def publish(self, client, plan=None):
        return RELEASE.publish_plan(client, plan or self.plan, self.asset_root, self.notes)

    def source_zip(self, entries):
        path = self.root / ("source-" + str(len(list(self.root.glob("source-*.zip")))) + ".zip")
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(path, "x") as output:
                for name, body, mode in entries:
                    entry = zipfile.ZipInfo(name)
                    # Preserve the raw central-directory spelling on Windows as well.
                    entry.filename = name
                    entry.create_system = 3
                    entry.external_attr = mode << 16
                    output.writestr(entry, body)
        return path

    def test_tag_requires_exact_stable_cargo_version(self):
        RELEASE.validate_tag("v1.0.0", "1.0.0")
        for tag, version in [
            ("v1.0.1", "1.0.0"), ("1.0.0", "1.0.0"), ("v01.0.0", "01.0.0"),
            ("v1.0.0-beta", "1.0.0-beta"), ("v1.0.0/extra", "1.0.0"),
            ("v1.0.0\n", "1.0.0"), ("v1.0.0$(evil)", "1.0.0"),
        ]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                RELEASE.validate_tag(tag, version)

    def test_prepare_rejects_checkout_tag_trigger_and_cargo_mismatches(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.0.0"\n', encoding="utf-8")
        self.changelog()
        context = self.root / "context.json"
        for tag_commit, triggering, dirty in [
            (OTHER_COMMIT, COMMIT, ""), (COMMIT, OTHER_COMMIT, ""), (COMMIT, COMMIT, " M src/main.rs"),
        ]:
            responses = {
                ("git", "rev-parse", "HEAD"): COMMIT,
                ("git", "rev-parse", "--verify", "v1.0.0^{commit}"): tag_commit,
                ("git", "status", "--porcelain", "--untracked-files=no"): dirty,
            }
            with self.subTest(tag_commit=tag_commit, triggering=triggering, dirty=dirty):
                with mock.patch.object(RELEASE, "ROOT", self.root), \
                        mock.patch.object(RELEASE, "CONTEXT_PATH", context), \
                        mock.patch.object(RELEASE, "run", side_effect=lambda args: responses[tuple(args)]):
                    with self.assertRaises(ValueError):
                        RELEASE.prepare("v1.0.0", triggering)
                self.assertFalse(context.exists())
        with mock.patch.object(RELEASE, "ROOT", self.root), mock.patch.object(RELEASE, "run") as command:
            with self.assertRaises(ValueError):
                RELEASE.prepare("v1.1.0", COMMIT)
            command.assert_not_called()

    def test_prepare_records_matching_commit_without_network(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.0.0"\n', encoding="utf-8")
        self.changelog()
        responses = {
            ("git", "rev-parse", "HEAD"): COMMIT,
            ("git", "rev-parse", "--verify", "v1.0.0^{commit}"): COMMIT,
            ("git", "status", "--porcelain", "--untracked-files=no"): "",
            ("git", "merge-base", "--is-ancestor", COMMIT, "origin/main"): "",
            ("rustc", "--version"): "synthetic rustc",
        }
        context = self.root / "context.json"
        with mock.patch.object(RELEASE, "ROOT", self.root), \
                mock.patch.object(RELEASE, "CONTEXT_PATH", context), \
                mock.patch.object(RELEASE, "run", side_effect=lambda args: responses[tuple(args)]), \
                contextlib.redirect_stdout(io.StringIO()):
            RELEASE.prepare("v1.0.0", COMMIT)
        self.assertIn(COMMIT, context.read_text(encoding="utf-8"))

    def test_prepare_stops_when_tag_is_not_on_main(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.0.0"\n', encoding="utf-8")
        self.changelog()
        responses = {
            ("git", "rev-parse", "HEAD"): COMMIT,
            ("git", "rev-parse", "--verify", "v1.0.0^{commit}"): COMMIT,
            ("git", "status", "--porcelain", "--untracked-files=no"): "",
        }

        def command(args):
            if args[:3] == ["git", "merge-base", "--is-ancestor"]:
                raise RuntimeError("git failed: not an ancestor")
            return responses[tuple(args)]
        context = self.root / "context.json"
        with mock.patch.object(RELEASE, "ROOT", self.root), \
                mock.patch.object(RELEASE, "CONTEXT_PATH", context), \
                mock.patch.object(RELEASE, "run", side_effect=command):
            with self.assertRaises(RuntimeError):
                RELEASE.prepare("v1.0.0", COMMIT)
        self.assertFalse(context.exists())

    def test_prepare_requires_release_notes_before_any_command(self):
        (self.root / "Cargo.toml").write_text('[package]\nversion = "1.0.0"\n', encoding="utf-8")
        self.changelog(CHANGELOG.replace("## [1.0.0] - 2026-10-05", "## [1.0.2] - 2026-10-05"))
        context = self.root / "context.json"
        with mock.patch.object(RELEASE, "ROOT", self.root), \
                mock.patch.object(RELEASE, "CONTEXT_PATH", context), \
                mock.patch.object(RELEASE, "run") as command:
            with self.assertRaises(ValueError):
                RELEASE.prepare("v1.0.0", COMMIT)
            command.assert_not_called()
        self.assertFalse(context.exists())

    def test_release_notes_use_only_the_matching_changelog_section(self):
        notes = RELEASE.release_notes(CHANGELOG.replace("\n", "\r\n"), "1.0.0", COMMIT)
        self.assertIn("合成的首版功能。", notes)
        self.assertIn("### Added", notes)
        self.assertNotIn("合成的下一版修正。", notes)
        self.assertNotIn("合成的前一版內容。", notes)
        self.assertNotIn("## [", notes)
        self.assertIn("來源提交：`" + COMMIT + "`", notes)
        self.assertIn("SHA256SUMS.txt", notes)
        self.assertNotIn("同列於附件", notes)
        self.assertIn("合成的前一版內容。", RELEASE.release_notes(CHANGELOG, "0.9.0", COMMIT))

    def test_release_notes_reject_missing_empty_or_duplicate_sections(self):
        for name, text in [
            ("missing", CHANGELOG),
            ("empty", CHANGELOG.replace("- 合成的前一版內容。\n", "\n  \n")),
            ("duplicate", CHANGELOG + "\n## [0.9.0] - 2026-09-02\n\n- 重複。\n"),
        ]:
            version = "2.0.0" if name == "missing" else "0.9.0"
            with self.subTest(name=name), self.assertRaises(ValueError):
                RELEASE.release_notes(text, version, COMMIT)

    def test_asset_verification_rejects_missing_extra_duplicate_size_hash_and_starter(self):
        RELEASE.verify_assets(self.uploaded(), self.expected)
        invalid = [
            self.uploaded()[:-1],
            self.uploaded() + [{"name": "unexpected.zip"}],
            self.uploaded() + [self.uploaded()[0]],
        ]
        for field, value in [("size", 0), ("digest", "sha256:" + "0" * 64), ("state", "starter")]:
            assets = self.uploaded()
            assets[0][field] = value
            invalid.append(assets)
        for actual in invalid:
            with self.subTest(actual=actual), self.assertRaises(ValueError):
                RELEASE.verify_assets(actual, self.expected)

    def test_expected_asset_plan_cannot_contain_duplicate_names(self):
        with self.assertRaises(ValueError):
            RELEASE.verify_assets(self.uploaded(), self.expected + [copy.deepcopy(self.expected[0])])

    def test_source_zip_extracts_into_only_expected_root(self):
        source = self.source_zip([(SOURCE_ROOT + "/Cargo.toml", b"synthetic", stat.S_IFREG)])
        destination = self.root / "extracted"
        directory, count = RELEASE.extract_source(source, destination, SOURCE_ROOT)
        self.assertEqual(directory, destination / SOURCE_ROOT)
        self.assertEqual(count, 1)
        self.assertEqual((directory / "Cargo.toml").read_bytes(), b"synthetic")

    def test_source_zip_rejects_traversal_symlinks_duplicates_and_wrong_root(self):
        entries = [
            [(SOURCE_ROOT + "/../outside", b"x", stat.S_IFREG)],
            [("/" + SOURCE_ROOT + "/absolute", b"x", stat.S_IFREG)],
            [(SOURCE_ROOT + "/drive:C", b"x", stat.S_IFREG)],
            [(SOURCE_ROOT + "\\outside", b"x", stat.S_IFREG)],
            [(SOURCE_ROOT + "/link", b"elsewhere", stat.S_IFLNK)],
            [("wrong-root/Cargo.toml", b"x", stat.S_IFREG)],
            [(SOURCE_ROOT + "/Cargo.toml", b"x", stat.S_IFREG),
             (SOURCE_ROOT + "/cargo.TOML", b"y", stat.S_IFREG)],
            [(SOURCE_ROOT + "/Cargo.toml", b"x", stat.S_IFREG),
             (SOURCE_ROOT + "/Cargo.toml", b"y", stat.S_IFREG)],
        ]
        for case in entries:
            with self.subTest(case=case), self.assertRaises(ValueError):
                RELEASE.extract_source(self.source_zip(case), self.root / "unsafe", SOURCE_ROOT)
        self.assertFalse((self.root / "outside").exists())
        self.assertFalse((self.root / "unsafe").exists())

    def test_source_zip_rejects_normalized_path_alias_duplicates(self):
        for alias in ["/docs/./NOTICE", "/docs//NOTICE"]:
            source = self.source_zip([
                (SOURCE_ROOT + "/docs/NOTICE", b"original", stat.S_IFREG),
                (SOURCE_ROOT + alias, b"replacement", stat.S_IFREG),
            ])
            with self.subTest(alias=alias), self.assertRaises(ValueError):
                RELEASE.extract_source(source, self.root / "alias", SOURCE_ROOT)

    def test_new_release_is_draft_until_all_attachments_pass_verification(self):
        client = FakeGhClient()
        release = self.publish(client)
        events = [event[0] for event in client.events]
        self.assertFalse(release["draft"])
        self.assertEqual(events.count("create"), 1)
        self.assertEqual(events.count("upload"), len(self.expected))
        self.assertLess(events.index("create"), events.index("upload"))
        publish = events.index("publish")
        self.assertEqual(events[publish - 1], "assets")
        self.assertEqual(events[publish + 1:], ["release_by_id", "assets"])
        RELEASE.verify_assets(client.uploaded, self.expected)

    def test_existing_draft_only_adds_missing_attachments(self):
        client = FakeGhClient(self.metadata(), self.uploaded()[:1])
        self.publish(client)
        self.assertEqual([event[2] for event in client.writes() if event[0] == "upload"],
                         [asset["name"] for asset in self.expected[1:]])
        self.assertNotIn("create", [event[0] for event in client.events])

    def test_new_draft_hidden_from_tag_lookup_publishes_using_creation_id(self):
        client = FakeGhClient()
        client.release = mock.Mock(return_value=None)
        release = self.publish(client)
        self.assertFalse(release["draft"])
        client.release.assert_called_once_with(self.plan["tag"])
        self.assertIn(("release_by_id", release["id"]), client.events)
        RELEASE.verify_assets(client.uploaded, self.expected)

    def test_wrong_remote_tag_or_changed_local_attachment_performs_no_write(self):
        client = FakeGhClient(commit=OTHER_COMMIT)
        with self.assertRaises(ValueError):
            self.publish(client)
        self.assertEqual(client.writes(), [])
        for field, value in [("tag_name", "v1.1.0"), ("prerelease", True)]:
            metadata = self.metadata()
            metadata[field] = value
            client = FakeGhClient(metadata, self.uploaded())
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.publish(client)
            self.assertEqual(client.writes(), [])
        client = FakeGhClient()
        (self.asset_root / self.expected[0]["name"]).write_bytes(b"modified after verification")
        with self.assertRaises(ValueError):
            self.publish(client)
        self.assertEqual(client.writes(), [])

    def test_mismatched_existing_draft_asset_is_replaced_before_publishing(self):
        assets = self.uploaded()
        assets[0]["digest"] = "sha256:" + "0" * 64
        assets[1]["state"] = "open"
        client = FakeGhClient(self.metadata(), assets)
        release = self.publish(client)
        self.assertFalse(release["draft"])
        writes = client.writes()
        replaced = [self.expected[0]["name"], self.expected[1]["name"]]
        self.assertEqual([event[1] for event in writes if event[0] == "delete"], replaced)
        self.assertEqual([event[2] for event in writes if event[0] == "upload"], replaced)
        for name in replaced:
            self.assertLess(writes.index(("delete", name)), writes.index(("upload", self.plan["tag"], name)))
        self.assertEqual(writes[-1], ("publish", self.plan["tag"]))
        RELEASE.verify_assets(client.uploaded, self.expected)

    def test_changed_published_release_reports_published_state_without_writes(self):
        assets = self.uploaded()
        assets[0]["digest"] = "sha256:" + "0" * 64
        client = FakeGhClient(self.metadata(draft=False), assets)
        with self.assertRaises(RELEASE.PublishedReleaseMismatch) as raised:
            self.publish(client)
        self.assertEqual(client.writes(), [])
        self.assertEqual(client.uploaded, assets)
        self.assertIn("already published", str(raised.exception))
        self.assertNotIn("draft", str(raised.exception).lower())

    def test_corrupt_new_upload_stays_draft(self):
        client = FakeGhClient()
        client.corrupt_upload = True
        with self.assertRaises(ValueError):
            self.publish(client)
        self.assertTrue(client.value["draft"])
        self.assertNotIn("publish", [event[0] for event in client.events])

    def test_same_published_release_is_read_only_and_idempotent(self):
        client = FakeGhClient(self.metadata(draft=False), self.uploaded())
        self.assertFalse(self.publish(client)["draft"])
        self.assertFalse(self.publish(client)["draft"])
        self.assertEqual(client.writes(), [])

    def test_changed_published_release_is_not_modified(self):
        client = FakeGhClient(self.metadata(draft=False), self.uploaded()[:-1])
        with self.assertRaises(ValueError):
            self.publish(client)
        self.assertEqual(client.writes(), [])

    def test_source_zip_expansion_limit_is_checked_before_extraction(self):
        body = b"synthetic data"
        source = self.source_zip([(SOURCE_ROOT + "/Cargo.toml", body, stat.S_IFREG)])
        destination = self.root / "too-large"
        with mock.patch.object(RELEASE, "MAX_SOURCE_EXPANDED_BYTES", len(body) - 1):
            with self.assertRaises(ValueError):
                RELEASE.extract_source(source, destination, SOURCE_ROOT)
        self.assertFalse(destination.exists())

    def test_offline_source_check_requires_gpl_files_and_removes_network_tokens(self):
        files = {
            "Cargo.toml": b'[package]\nversion = "1.0.0"\n',
            "vendor/rust/synthetic/Cargo.toml": b"synthetic vendored dependency",
            "LICENSE": b"synthetic Apache terms", "COPYING": b"synthetic GPL terms",
            "NOTICE": b"synthetic notice", "THIRD_PARTY_NOTICES.md": b"synthetic terms",
            "vendor/mediawiki/ZhConversion.php": b"synthetic data", "Cargo.lock": b"synthetic lock",
        }
        archive = self.source_zip([
            (SOURCE_ROOT + "/" + name, body, stat.S_IFREG) for name, body in files.items()
        ])
        with mock.patch.object(RELEASE, "ROOT", self.root), \
                mock.patch.object(RELEASE, "run", return_value="") as command, \
                mock.patch.dict("os.environ", {"GH_TOKEN": "<TEST_TOKEN>", "GITHUB_TOKEN": "<TEST_TOKEN>"}):
            result = RELEASE.verify_source(archive, "1.0.0")
        self.assertTrue(result["empty_cargo_home"])
        arguments, = command.call_args.args
        self.assertIn("--offline", arguments)
        self.assertIn("--locked", arguments)
        self.assertEqual(arguments[arguments.index("--target") + 1], RELEASE.RUST_TARGET)
        environment = command.call_args.kwargs["env"]
        self.assertNotIn("GH_TOKEN", environment)
        self.assertNotIn("GITHUB_TOKEN", environment)
        self.assertEqual(list(pathlib.Path(environment["CARGO_HOME"]).iterdir()), [])
        del files["COPYING"]
        incomplete = self.source_zip([
            (SOURCE_ROOT + "/" + name, body, stat.S_IFREG) for name, body in files.items()
        ])
        with mock.patch.object(RELEASE, "ROOT", self.root), mock.patch.object(RELEASE, "run") as command:
            with self.assertRaises(ValueError):
                RELEASE.verify_source(incomplete, "1.0.0")
            command.assert_not_called()

    def test_collect_pairs_source_portable_and_executable_hash_in_six_assets(self):
        build = self.root / "dist/rust-synthetic"
        portable_directory = build / "portable"
        portable_directory.mkdir(parents=True)
        executable = portable_directory / "SC2TC-Renamer.exe"
        executable.write_bytes(b"synthetic verified executable")
        source = build / "SC2TC-Renamer-v1.0.0-source.zip"
        portable = build / "SC2TC-Renamer-v1.0.0-portable.zip"
        with zipfile.ZipFile(source, "x") as output:
            output.writestr(SOURCE_ROOT + "/Cargo.toml", '[package]\nversion = "1.0.0"\n')
        with zipfile.ZipFile(portable, "x") as output:
            output.write(executable, executable.name)
        hashes = {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (source, portable, executable)
        }
        manifest = {
            "version": "1.0.0", "target": RELEASE.RUST_TARGET, "sha256": hashes,
            "app_name": "SC2TC-Renamer", "license": "GPL-3.0-only",
            "source_zip": str(source), "portable_zip": str(portable), "executable": str(executable),
        }
        (build / "release-manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
        context = self.root / "context.json"
        context.write_text(json.dumps({
            "version": "1.0.0", "tag": "v1.0.0", "source_commit": COMMIT,
            "target": RELEASE.RUST_TARGET,
        }), encoding="utf-8")
        assets = self.root / "work/collected-assets"
        plan_path = self.root / "work/plan.json"
        def synthetic_self_test(arguments, **_kwargs):
            self.assertEqual(arguments[:2], [str(executable), "--self-test"])
            fixture = pathlib.Path(arguments[2])
            fixture.mkdir(parents=True)
            (fixture / "verification.json").write_text(json.dumps({
                "version": "1.0.0", "engine": "MediaWiki",
                "original_names_restored": True, "sha256_unchanged": True,
                "modes": [{"mode": mode, "fixture": str(fixture / mode)} for mode in ("zh-Hant", "zh-TW")],
            }), encoding="utf-8")
            return ""
        with mock.patch.object(RELEASE, "ROOT", self.root), \
                mock.patch.object(RELEASE, "CONTEXT_PATH", context), \
                mock.patch.object(RELEASE, "ASSET_ROOT", assets), \
                mock.patch.object(RELEASE, "PLAN_PATH", plan_path), \
                mock.patch.object(RELEASE, "run", side_effect=synthetic_self_test), \
                mock.patch.object(RELEASE, "verify_source", return_value={
                    "source_version": "1.0.0", "offline_check_exit_code": 0,
                }) as offline, contextlib.redirect_stdout(io.StringIO()):
            RELEASE.collect()
        offline.assert_called_once_with(source, "1.0.0")
        expected_names = {
            source.name, portable.name, "verification.json", "source-verification.json",
            "release-manifest.json", "SHA256SUMS.txt",
        }
        plan = json.loads(plan_path.read_text(encoding="utf-8"))
        self.assertEqual({asset["name"] for asset in plan["assets"]}, expected_names)
        self.assertEqual(len(plan["assets"]), len(expected_names))
        for asset in plan["assets"]:
            path = assets / asset["name"]
            self.assertEqual(asset["digest"], "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(asset["size"], path.stat().st_size)
        checksums = (assets / "SHA256SUMS.txt").read_text(encoding="utf-8").splitlines()
        self.assertNotIn(hashes[executable.name] + "  " + executable.name, checksums)
        listed = {}
        for line in checksums:
            value, name = line.split("  ", 1)
            self.assertTrue((assets / name).is_file(), name)
            self.assertEqual(value, hashlib.sha256((assets / name).read_bytes()).hexdigest())
            listed[name] = value
        self.assertEqual(set(listed), expected_names - {"SHA256SUMS.txt"})
        self.assertEqual(len(listed), len(checksums))
        verification = json.loads((assets / "verification.json").read_text(encoding="utf-8"))
        self.assertEqual([mode["fixture"] for mode in verification["modes"]], ["zh-Hant", "zh-TW"])
        local = str(self.root)
        for path in assets.iterdir():
            if path.suffix in (".json", ".txt"):
                text = path.read_text(encoding="utf-8")
                with self.subTest(asset=path.name):
                    self.assertNotIn(local, text)
                    self.assertNotIn(json.dumps(local)[1:-1], text)
        self.assertEqual((assets / source.name).read_bytes(), source.read_bytes())
        self.assertEqual((assets / portable.name).read_bytes(), portable.read_bytes())
        public_manifest = json.loads((assets / "release-manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(public_manifest["source_zip"], source.name)
        self.assertEqual(public_manifest["source_commit"], COMMIT)
        self.assertEqual(public_manifest["app_name"], "SC2TC-Renamer")
        self.assertEqual(public_manifest["license"], "GPL-3.0-only")


if __name__ == "__main__":
    unittest.main()
