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
        self.uploaded.append({
            "name": path.name, "size": path.stat().st_size, "digest": digest, "state": "uploaded",
        })

    def publish(self, release):
        tag = release["tag_name"]
        self.events.append(("publish", tag))
        self.value["draft"] = False

    def writes(self):
        return [event for event in self.events if event[0] in ("create", "upload", "publish")]


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
        return [dict(asset, state="uploaded") for asset in self.expected]

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

    def test_mismatched_existing_draft_preserves_asset_and_never_publishes(self):
        assets = self.uploaded()
        assets[0]["digest"] = "sha256:" + "0" * 64
        client = FakeGhClient(self.metadata(), assets)
        with self.assertRaises(ValueError):
            self.publish(client)
        self.assertTrue(client.value["draft"])
        self.assertEqual(client.writes(), [])
        self.assertEqual(client.uploaded, assets)

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
        self.assertIn(hashes[executable.name] + "  " + executable.name, checksums)
        self.assertEqual((assets / source.name).read_bytes(), source.read_bytes())
        self.assertEqual((assets / portable.name).read_bytes(), portable.read_bytes())
        public_manifest = json.loads((assets / "release-manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(public_manifest["source_zip"], source.name)
        self.assertEqual(public_manifest["source_commit"], COMMIT)
        self.assertEqual(public_manifest["app_name"], "SC2TC-Renamer")
        self.assertEqual(public_manifest["license"], "GPL-3.0-only")


if __name__ == "__main__":
    unittest.main()
