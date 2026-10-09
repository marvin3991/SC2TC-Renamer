"""Build a frozen Rust source snapshot and ship its complete corresponding source."""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import sys
import tomllib
import uuid
import zipfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE_FILES = (
    'Cargo.toml', 'Cargo.lock', 'build.rs', 'build.ps1', 'build-rust.ps1',
    'README.md', 'CHANGELOG.md', 'LICENSE', 'COPYING', 'NOTICE', 'THIRD_PARTY_NOTICES.md',
    '.cargo/config.toml', '.gitattributes', 'scripts/collect-licenses.py', 'scripts/package-rust.py',
    'scripts/release-assets.py', '.github/workflows/release.yml',
    'tests/test_release.py', 'tests/test_package.py',
)
# examples/prepare_icon.rs generates assets/app.ico from assets/logo-v2.png.
SOURCE_DIRECTORIES = ('src', 'assets', 'licenses', 'docs', 'examples', 'vendor/mediawiki')
# File-manager metadata listed in .gitignore; a local snapshot must not ship it.
OS_METADATA_NAMES = ('thumbs.db', 'desktop.ini', '.ds_store')
OS_METADATA_PREFIXES = ('~$',)
# Values written by `cargo vendor --versioned-dirs vendor/rust`.
VENDORED_SOURCE_NAME = 'vendored-sources'
VENDORED_DIRECTORY = 'vendor/rust'
GITHUB_WARNING_BYTES = 50 * 1024 * 1024
RUST_TARGET = 'x86_64-pc-windows-msvc'


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def is_os_metadata(path):
    name = path.name.casefold()
    return name in OS_METADATA_NAMES or name.startswith(OS_METADATA_PREFIXES)


def require_vendored_configuration(configuration):
    """Fail unless Cargo is routed to vendor/rust, so the source ZIP rebuilds offline."""
    try:
        settings = tomllib.loads(configuration.read_text(encoding='utf-8'))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError('Cannot read Cargo configuration: ' + str(configuration)) from error
    sources = settings.get('source')
    sources = sources if isinstance(sources, dict) else {}
    crates_io = sources.get('crates-io')
    vendored = sources.get(VENDORED_SOURCE_NAME)
    if (not isinstance(crates_io, dict) or crates_io.get('replace-with') != VENDORED_SOURCE_NAME
            or not isinstance(vendored, dict) or vendored.get('directory') != VENDORED_DIRECTORY):
        raise RuntimeError('vendor/rust exists but ' + str(configuration) + ' does not replace crates-io with '
                           + VENDORED_SOURCE_NAME + ' at ' + VENDORED_DIRECTORY
                           + '; remove the pre-vendored directory or add the cargo vendor configuration.')


def source_files():
    files = {ROOT / name for name in SOURCE_FILES}
    for directory in SOURCE_DIRECTORIES:
        files.update(path for path in (ROOT / directory).rglob('*') if path.is_file())
    files.update((ROOT / 'tests').rglob('*.rs'))
    if (ROOT / 'vendor' / 'rust').is_dir():
        files.update(path for path in (ROOT / 'vendor' / 'rust').rglob('*') if path.is_file())
    for path in files:
        if path.is_symlink() or not path.is_file():
            raise RuntimeError('Required source is missing or is a symlink: ' + str(path))
        if path.name.startswith('.env') or path.suffix.lower() in ('.exe', '.log'):
            raise RuntimeError('Unexpected private or binary input: ' + str(path))
        if is_os_metadata(path):
            raise RuntimeError('Unexpected file-manager metadata; remove it before packaging: ' + str(path))
    return sorted(files)


def run(command, cwd, log):
    completed = subprocess.run(command, cwd=cwd, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding='utf-8', errors='replace')
    log.write_text(completed.stdout + completed.stderr, encoding='utf-8')
    if completed.returncode:
        raise RuntimeError(f'Command failed ({completed.returncode}); see {log}')
    return completed.stdout


def archive(source, output, top_directory):
    with zipfile.ZipFile(output, 'x', compression=zipfile.ZIP_DEFLATED,
                         strict_timestamps=False) as target:
        for path in sorted(source.rglob('*')):
            if path.is_file():
                target.write(path, top_directory + '/' + path.relative_to(source).as_posix())
    with zipfile.ZipFile(output) as check:
        bad = check.testzip()
        if bad:
            raise RuntimeError('ZIP verification failed: ' + bad)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot-only', action='store_true', help='Validate and copy build inputs without building.')
    arguments = parser.parse_args()
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    destination = ROOT / 'dist' / ('rust-' + version + '-' + uuid.uuid4().hex)
    destination.mkdir(parents=True, exist_ok=False)
    snapshot = destination / ('SC2TC-Renamer-' + version + '-source')
    snapshot.mkdir()
    files = source_files()
    hashes = {str(path.relative_to(ROOT)).replace('\\', '/'): digest(path) for path in files}
    for path in files:
        output = snapshot / path.relative_to(ROOT)
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, output)
    # Do not pair an executable with a mixture of old and concurrently edited source.
    if files != source_files() or any(digest(path) != hashes[str(path.relative_to(ROOT)).replace('\\', '/')] for path in files):
        raise RuntimeError('Source changed while snapshotting; keep this output for diagnosis and rerun once edits finish.')
    if any(digest(snapshot / name) != expected for name, expected in hashes.items()):
        raise RuntimeError('Source snapshot verification failed.')
    (destination / 'input-source-sha256.json').write_text(json.dumps(hashes, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'stage': 'snapshot', 'directory': str(destination), 'files': len(files)}, ensure_ascii=False), flush=True)
    if arguments.snapshot_only:
        return

    if not (snapshot / 'vendor' / 'rust').is_dir():
        vendor_configuration = run(['cargo', 'vendor', '--locked', '--versioned-dirs', 'vendor/rust'],
                                   snapshot, destination / 'cargo-vendor.log')
        if '[source.vendored-sources]' not in vendor_configuration or 'directory = "vendor/rust"' not in vendor_configuration:
            raise RuntimeError('Unexpected cargo vendor configuration.')
        configuration = snapshot / '.cargo' / 'config.toml'
        configuration.write_text(configuration.read_text(encoding='utf-8') + '\n' + vendor_configuration, encoding='utf-8')
    # A pre-vendored tree is only usable offline when the shipped configuration points at it.
    require_vendored_configuration(snapshot / '.cargo' / 'config.toml')
    print(json.dumps({'stage': 'vendor', 'directory': str(destination)}, ensure_ascii=False), flush=True)

    run([sys.executable, 'scripts/collect-licenses.py'], snapshot, destination / 'collect-licenses.log')
    # Dependencies are built from the vendored source, not from whichever registry
    # version happens to be present on the build computer.
    build_target = destination / 'build-target'
    run(['cargo', 'build', '--release', '--locked', '--offline', '--target', RUST_TARGET,
         '--target-dir', str(build_target)],
        snapshot, destination / 'cargo-build.log')
    portable = destination / ('SC2TC-Renamer-' + version + '-portable')
    portable.mkdir()
    shutil.copyfile(build_target / RUST_TARGET / 'release' / 'SC2TC-Renamer.exe', portable / 'SC2TC-Renamer.exe')
    for filename in ('README.md', 'LICENSE', 'COPYING', 'NOTICE', 'THIRD_PARTY_NOTICES.md'):
        shutil.copyfile(snapshot / filename, portable / filename)
    shutil.copytree(snapshot / 'licenses', portable / 'licenses')
    shutil.copytree(snapshot / 'docs', portable / 'docs')
    portable_metadata = portable / 'vendor' / 'mediawiki'
    portable_metadata.mkdir(parents=True)
    shutil.copyfile(snapshot / 'vendor' / 'mediawiki' / 'manifest.json', portable_metadata / 'manifest.json')
    source_zip = destination / ('SC2TC-Renamer-v' + version + '-source.zip')
    portable_zip = destination / ('SC2TC-Renamer-v' + version + '-portable.zip')
    archive(snapshot, source_zip, snapshot.name)
    archive(portable, portable_zip, portable.name)
    artifact_hashes = {path.name: digest(path) for path in (source_zip, portable_zip)}
    artifact_hashes['SC2TC-Renamer.exe'] = digest(portable / 'SC2TC-Renamer.exe')
    (destination / 'SHA256SUMS.txt').write_text(''.join(f'{value}  {name}\n' for name, value in artifact_hashes.items()), encoding='utf-8')
    report = {
        'version': version,
        'license': 'GPL-3.0-only',
        'author_source_license': 'Apache-2.0',
        'target': RUST_TARGET,
        'source_zip': str(source_zip),
        'portable_zip': str(portable_zip),
        'executable': str(portable / 'SC2TC-Renamer.exe'),
        'build': 'cargo build --release --locked --offline --target x86_64-pc-windows-msvc, using the shipped snapshot and vendored crates',
        'sha256': artifact_hashes,
        'bytes': {path.name: path.stat().st_size for path in (source_zip, portable_zip)},
        'large_artifacts_for_review': [path.name for path in (source_zip, portable_zip) if path.stat().st_size > GITHUB_WARNING_BYTES],
        'app_name': 'SC2TC-Renamer',
    }
    (destination / 'release-manifest.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, ensure_ascii=False), flush=True)


if __name__ == '__main__':
    main()
