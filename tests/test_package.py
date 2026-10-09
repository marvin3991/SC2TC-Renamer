"""Offline checks for the local source snapshot used by build.ps1."""
import importlib.util
import pathlib
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("package_rust_under_test", ROOT / "scripts/package-rust.py")
PACKAGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGE)
VENDORED_CONFIGURATION = (
    '[target.x86_64-pc-windows-msvc]\n'
    'rustflags = ["-C", "target-feature=+crt-static"]\n\n'
    '[source.crates-io]\n'
    'replace-with = "vendored-sources"\n\n'
    '[source.vendored-sources]\n'
    'directory = "vendor/rust"\n'
)


class PackageTests(unittest.TestCase):
    def setUp(self):
        work = ROOT / "work/package-tests"
        work.mkdir(parents=True, exist_ok=True)
        folder = tempfile.TemporaryDirectory(prefix="fixture-", dir=work)
        self.addCleanup(folder.cleanup)
        self.root = pathlib.Path(folder.name)

    def configuration(self, text):
        path = self.root / "config.toml"
        path.write_text(text, encoding="utf-8")
        return path

    def test_pre_vendored_tree_requires_crates_io_replacement(self):
        PACKAGE.require_vendored_configuration(self.configuration(VENDORED_CONFIGURATION))
        for name, text in [
            ("repository default without source replacement",
             '[target.x86_64-pc-windows-msvc]\nrustflags = ["-C", "target-feature=+crt-static"]\n'),
            ("replacement pointing at another directory",
             VENDORED_CONFIGURATION.replace('"vendor/rust"', '"vendor"')),
            ("vendored directory without crates-io replacement",
             VENDORED_CONFIGURATION.replace('replace-with = "vendored-sources"\n', '')),
            ("malformed TOML", '[source.crates-io\n'),
        ]:
            with self.subTest(name=name), self.assertRaises(RuntimeError):
                PACKAGE.require_vendored_configuration(self.configuration(text))

    def tree(self, *names):
        for name in names:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"synthetic")

    def snapshot_files(self):
        with mock.patch.object(PACKAGE, "ROOT", self.root), \
                mock.patch.object(PACKAGE, "SOURCE_FILES", ("Cargo.toml",)), \
                mock.patch.object(PACKAGE, "SOURCE_DIRECTORIES", ("docs",)):
            return PACKAGE.source_files()

    def test_clean_tree_is_accepted(self):
        self.tree("Cargo.toml", "docs/BUILD.md", "docs/images/interface.png", "tests/core_rust.rs")
        self.assertEqual(
            [path.relative_to(self.root).as_posix() for path in self.snapshot_files()],
            ["Cargo.toml", "docs/BUILD.md", "docs/images/interface.png", "tests/core_rust.rs"],
        )

    def test_file_manager_metadata_is_rejected_case_insensitively(self):
        for name in ["docs/images/Thumbs.db", "docs/THUMBS.DB", "docs/Desktop.ini",
                     "docs/.DS_Store", "docs/~$BUILD.md"]:
            with self.subTest(name=name):
                self.tree("Cargo.toml", "docs/BUILD.md", name)
                with self.assertRaises(RuntimeError) as raised:
                    self.snapshot_files()
                self.assertIn("metadata", str(raised.exception))
                (self.root / name).unlink()

    def test_snapshot_collects_icon_generator_from_examples(self):
        self.tree("Cargo.toml", "examples/prepare_icon.rs")
        with mock.patch.object(PACKAGE, "ROOT", self.root), \
                mock.patch.object(PACKAGE, "SOURCE_FILES", ("Cargo.toml",)), \
                mock.patch.object(PACKAGE, "SOURCE_DIRECTORIES", ("examples",)):
            files = PACKAGE.source_files()
        self.assertIn(self.root / "examples" / "prepare_icon.rs", files)

    def test_repository_ships_icon_generator_directory(self):
        self.assertIn("examples", PACKAGE.SOURCE_DIRECTORIES)
        self.assertTrue((ROOT / "examples" / "prepare_icon.rs").is_file())

    def test_portable_documents_include_changelog_from_snapshot(self):
        self.assertIn("CHANGELOG.md", PACKAGE.PORTABLE_FILES)
        for name in PACKAGE.PORTABLE_FILES:
            with self.subTest(name=name):
                self.assertIn(name, PACKAGE.SOURCE_FILES)


if __name__ == "__main__":
    unittest.main()
