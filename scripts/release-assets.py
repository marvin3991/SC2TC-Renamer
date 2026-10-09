"""Validate a version tag, verify build assets, then publish a checked Release draft."""
import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from urllib.parse import quote

ROOT = pathlib.Path(__file__).resolve().parents[1]
RUST_TARGET = 'x86_64-pc-windows-msvc'
RELEASE_TAG_PATTERN = re.compile(r'v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)')
MAX_SOURCE_EXPANDED_BYTES = 1024 * 1024 * 1024
MAX_NESTED_TAGS = 5
CONTEXT_PATH = ROOT / 'work' / 'release-context.json'
ASSET_ROOT = ROOT / 'work' / 'release-assets'
PLAN_PATH = ROOT / 'work' / 'release-plan.json'
# Keep a Changelog version headings, for example "## [1.1.0] - 2026-10-09".
CHANGELOG_HEADING = re.compile(r'^## \[([^\]]+)\][^\n]*$', re.MULTILINE)
# Checked by .github/workflows/release.yml to tell a published release apart from a kept draft.
PUBLISHED_MISMATCH_EXIT_CODE = 3


class PublishedReleaseMismatch(ValueError):
    """The tag already has a published release whose attachments differ from this build."""


def run(command, cwd=ROOT, env=None, log=None):
    result = subprocess.run(command, cwd=cwd, env=env, capture_output=True,
                            text=True, encoding='utf-8', errors='replace')
    if log is not None:
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text(result.stdout + result.stderr, encoding='utf-8')
    if result.returncode:
        raise RuntimeError(f'{command[0]} failed: {result.stderr.strip()}')
    return result.stdout.strip()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('x', encoding='utf-8', newline='\n') as output:
        json.dump(value, output, ensure_ascii=False, indent=2)
        output.write('\n')


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def validate_tag(tag, version):
    if not RELEASE_TAG_PATTERN.fullmatch(tag) or tag != 'v' + version:
        raise ValueError('Tag must equal v plus the stable Cargo package version.')


def release_notes(changelog_text, version, source_commit):
    """Return the Release body: this version's CHANGELOG section plus fixed source and licence notes."""
    text = changelog_text.replace('\r\n', '\n')
    headings = list(CHANGELOG_HEADING.finditer(text))
    sections = [index for index, heading in enumerate(headings) if heading.group(1) == version]
    if len(sections) != 1:
        raise ValueError('CHANGELOG.md must contain exactly one section for version ' + version + '.')
    index = sections[0]
    end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
    body = text[headings[index].end():end].strip()
    if not body:
        raise ValueError('CHANGELOG.md section for version ' + version + ' is empty.')
    return ('純 MediaWiki 檔名轉換，提供 zh-Hant／zh-TW。下載 portable ZIP 後解壓執行。\n\n'
            + body + '\n\n---\n\n'
            '授權全文（LICENSE、COPYING、NOTICE、THIRD_PARTY_NOTICES.md、licenses/）在 portable 與 source ZIP 內；'
            '附件 SHA-256 見 SHA256SUMS.txt，執行檔 SHA-256 見 release-manifest.json。\n'
            '作者原始碼為 Apache 2.0；整體程式含 GPL 轉換表，依 GPL 第 3 版交付。\n\n'
            f'來源提交：`{source_commit}`。下載與存放路徑詳見本版本 README。\n')


def prepare(tag, expected_sha):
    version = tomllib.loads((ROOT / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    validate_tag(tag, version)
    # Fail before the long build when the version has no release notes.
    release_notes((ROOT / 'CHANGELOG.md').read_text(encoding='utf-8'), version, expected_sha)
    head = run(['git', 'rev-parse', 'HEAD'])
    target = run(['git', 'rev-parse', '--verify', tag + '^{commit}'])
    if head != target or head != expected_sha:
        raise ValueError('Checkout, tag and triggering GitHub commit must be identical.')
    if run(['git', 'status', '--porcelain', '--untracked-files=no']):
        raise ValueError('Release requires an unchanged tracked checkout.')
    run(['git', 'merge-base', '--is-ancestor', head, 'origin/main'])
    context = {'tag': tag, 'version': version, 'source_commit': head,
               'target': RUST_TARGET, 'rustc': run(['rustc', '--version'])}
    write_json(CONTEXT_PATH, context)
    print(json.dumps(context), flush=True)


def extract_source(archive, destination, expected_root):
    total = 0
    seen = set()
    with zipfile.ZipFile(archive) as source:
        if source.testzip() is not None:
            raise ValueError('Source ZIP CRC check failed.')
        for entry in source.infolist():
            # ZipInfo.filename normalizes backslashes on Windows; inspect the stored spelling.
            raw_name = entry.orig_filename
            path = pathlib.PurePosixPath(raw_name)
            raw_parts = raw_name.split('/')
            if entry.is_dir():
                raw_parts = raw_parts[:-1]
            if (path.is_absolute() or not path.parts or path.parts[0] != expected_root
                    or any(part in ('', '.', '..') or ':' in part for part in raw_parts)
                    or '\\' in raw_name or any(ord(char) < 32 for char in raw_name)
                    or stat.S_ISLNK(entry.external_attr >> 16)):
                raise ValueError('Unsafe source archive entry.')
            key = path.as_posix().casefold()
            if key in seen:
                raise ValueError('Duplicate source archive entry.')
            seen.add(key)
            total += entry.file_size
            if total > MAX_SOURCE_EXPANDED_BYTES:
                raise ValueError('Source archive expanded size exceeds the limit.')
        source.extractall(destination)
    return destination / expected_root, len(seen)


def verify_source(archive, version):
    parent = ROOT / 'work'
    parent.mkdir(exist_ok=True)
    # Keep verification files and logs for diagnosing an interrupted run.
    base = pathlib.Path(tempfile.mkdtemp(prefix='release-source-check-', dir=parent))
    source_root, count = extract_source(archive, base, 'SC2TC-Renamer-' + version + '-source')
    source_version = tomllib.loads((source_root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    if source_version != version or not (source_root / 'vendor/rust').is_dir():
        raise ValueError('Source archive version or vendored dependencies are missing.')
    for filename in ('LICENSE', 'COPYING', 'NOTICE', 'THIRD_PARTY_NOTICES.md',
                     'vendor/mediawiki/ZhConversion.php', 'Cargo.lock'):
        if not (source_root / filename).is_file():
            raise ValueError('Corresponding source is missing ' + filename)
    cargo_home = base / 'empty-cargo-home'
    cargo_home.mkdir()
    env = os.environ.copy()
    env['CARGO_HOME'] = str(cargo_home)
    env.pop('GH_TOKEN', None)
    env.pop('GITHUB_TOKEN', None)
    run(['cargo', 'check', '--all-targets', '--locked', '--offline', '--target', RUST_TARGET,
         '--target-dir', str(base / 'check-target')], cwd=source_root, env=env,
        log=base / 'cargo-check-offline.log')
    return {'source_files': count, 'source_version': version, 'offline_check_exit_code': 0,
            'empty_cargo_home': True, 'target': RUST_TARGET}


def collect():
    context = json.loads(CONTEXT_PATH.read_text(encoding='utf-8'))
    candidates = list((ROOT / 'dist').glob('rust-*/release-manifest.json'))
    if len(candidates) != 1:
        raise ValueError('Release requires exactly one build output in this clean runner.')
    manifest_path = candidates[0]
    manifest = json.loads(manifest_path.read_text(encoding='utf-8'))
    version = context['version']
    if manifest['version'] != version or manifest.get('target') != RUST_TARGET:
        raise ValueError('Built package version or target differs from the tag.')
    source = pathlib.Path(manifest['source_zip']).resolve(strict=True)
    portable = pathlib.Path(manifest['portable_zip']).resolve(strict=True)
    executable = pathlib.Path(manifest['executable']).resolve(strict=True)
    output_root = manifest_path.parent.resolve()
    for path in (source, portable, executable):
        if not path.is_relative_to(output_root) or path.is_symlink():
            raise ValueError('Build artifact escapes the build output directory.')
        if digest(path) != manifest['sha256'][path.name]:
            raise ValueError('Build artifact checksum mismatch: ' + path.name)
    ASSET_ROOT.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(source, ASSET_ROOT / source.name)
    shutil.copyfile(portable, ASSET_ROOT / portable.name)
    fixture = ROOT / 'work' / 'release-executable-check'
    run([str(executable), '--self-test', str(fixture)], log=ROOT / 'work/release-executable.log')
    verification = json.loads((fixture / 'verification.json').read_text(encoding='utf-8'))
    if (verification.get('version') != version or verification.get('engine') != 'MediaWiki'
            or not verification.get('original_names_restored') or not verification.get('sha256_unchanged')):
        raise ValueError('Executable file operations did not pass.')
    # Public metadata uses asset and fixture folder names, not runner filesystem locations;
    # the full paths stay in work/ for diagnosis.
    for mode in verification.get('modes', []):
        if isinstance(mode, dict) and 'fixture' in mode:
            mode['fixture'] = pathlib.PureWindowsPath(str(mode['fixture'])).name
    write_json(ASSET_ROOT / 'verification.json', verification)
    write_json(ASSET_ROOT / 'source-verification.json', verify_source(source, version))
    manifest.update(context)
    manifest['source_zip'], manifest['portable_zip'], manifest['executable'] = source.name, portable.name, executable.name
    write_json(ASSET_ROOT / 'release-manifest.json', manifest)
    paths = sorted(ASSET_ROOT.iterdir())
    # Only attachments are listed so `sha256sum -c` works in the download folder; the
    # executable inside the portable ZIP is hashed in release-manifest.json.
    with (ASSET_ROOT / 'SHA256SUMS.txt').open('x', encoding='utf-8', newline='\n') as stream:
        stream.write(''.join(f'{digest(path)}  {path.name}\n' for path in paths))
    plan = {'tag': context['tag'], 'source_commit': context['source_commit'], 'assets': []}
    for path in sorted(ASSET_ROOT.iterdir()):
        plan['assets'].append({'name': path.name, 'size': path.stat().st_size, 'digest': 'sha256:' + digest(path)})
    write_json(PLAN_PATH, plan)
    print(json.dumps({'tag': plan['tag'], 'assets': [asset['name'] for asset in plan['assets']]}), flush=True)


def assets_by_name(actual):
    by_name = {}
    for asset in actual:
        if asset['name'] in by_name:
            raise ValueError('Duplicate Release asset.')
        by_name[asset['name']] = asset
    return by_name


def asset_matches(uploaded, expected):
    return (uploaded.get('state') == 'uploaded' and uploaded.get('size') == expected['size']
            and uploaded.get('digest') == expected['digest'])


def verify_assets(actual, expected):
    expected_names = {asset['name'] for asset in expected}
    if len(expected_names) != len(expected):
        raise ValueError('Duplicate expected Release asset.')
    by_name = assets_by_name(actual)
    if set(by_name) != expected_names:
        raise ValueError('Release attachment set is incomplete or contains unexpected files.')
    for asset in expected:
        if not asset_matches(by_name[asset['name']], asset):
            raise ValueError('Release attachment verification failed: ' + asset['name'])


class GhClient:
    def __init__(self, repo):
        if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repo):
            raise ValueError('Invalid repository name.')
        self.repo = repo

    def api(self, endpoint):
        return json.loads(run(['gh', 'api', endpoint]))

    def release(self, tag):
        result = subprocess.run(['gh', 'api', f'repos/{self.repo}/releases/tags/{tag}'],
                                capture_output=True, text=True, encoding='utf-8')
        if result.returncode and 'HTTP 404' in result.stderr:
            # The tag endpoint omits drafts even for their creator; the release list includes them.
            pages = json.loads(run(['gh', 'api', f'repos/{self.repo}/releases?per_page=100',
                                    '--paginate', '--slurp']))
            matches = [release for page in pages for release in page if release['tag_name'] == tag]
            if len(matches) > 1:
                raise ValueError('Multiple releases use the same version tag.')
            return matches[0] if matches else None
        if result.returncode:
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)

    def assets(self, release):
        pages = json.loads(run(['gh', 'api', f'repos/{self.repo}/releases/{release["id"]}/assets?per_page=100',
                               '--paginate', '--slurp']))
        return [asset for page in pages for asset in page]

    def release_by_id(self, release_id):
        return self.api(f'repos/{self.repo}/releases/{release_id}')

    def tag_commit(self, tag):
        value = self.api(f'repos/{self.repo}/git/ref/tags/{tag}')['object']
        for _ in range(MAX_NESTED_TAGS):
            if value['type'] == 'commit':
                return value['sha']
            if value['type'] != 'tag':
                break
            value = self.api(f'repos/{self.repo}/git/tags/{value["sha"]}')['object']
        raise ValueError('Release tag does not resolve to a commit.')

    def create(self, tag, commit, notes):
        # Keep the POST response: integration tokens may not list their new draft immediately.
        payload = {'tag_name': tag, 'target_commitish': commit, 'draft': True,
                   'prerelease': False, 'name': 'SC2TC-Renamer ' + tag,
                   'body': notes.read_text(encoding='utf-8')}
        response = subprocess.run(['gh', 'api', f'repos/{self.repo}/releases', '--method', 'POST', '--input', '-'],
                                  input=json.dumps(payload), capture_output=True, text=True, encoding='utf-8')
        if response.returncode:
            raise RuntimeError(response.stderr.strip())
        return json.loads(response.stdout)

    def upload(self, release, path):
        endpoint = f'https://uploads.github.com/repos/{self.repo}/releases/{release["id"]}/assets?name={quote(path.name, safe="")}'
        run(['gh', 'api', endpoint, '--method', 'POST', '--header', 'Content-Type: application/octet-stream',
             '--input', str(path)])

    def delete_asset(self, asset):
        asset_id = asset.get('id')
        if not isinstance(asset_id, int) or isinstance(asset_id, bool):
            raise ValueError('Release asset has no numeric id.')
        run(['gh', 'api', f'repos/{self.repo}/releases/assets/{asset_id}', '--method', 'DELETE'])

    def publish(self, release):
        run(['gh', 'api', f'repos/{self.repo}/releases/{release["id"]}', '--method', 'PATCH', '-F', 'draft=false'])


def publish_plan(client, plan, asset_root, notes):
    tag = plan['tag']
    if client.tag_commit(tag) != plan['source_commit']:
        raise ValueError('Remote tag does not match the checked source commit.')
    for asset in plan['assets']:
        path = asset_root / asset['name']
        if path.name != asset['name'] or path.stat().st_size != asset['size'] or 'sha256:' + digest(path) != asset['digest']:
            raise ValueError('Local attachment changed after verification.')
    release = client.release(tag)
    if release is None:
        release = client.create(tag, plan['source_commit'], notes)
    if release is None or release['tag_name'] != tag or release.get('prerelease'):
        raise ValueError('Release metadata does not match the stable version tag.')
    actual = client.assets(release)
    if not release['draft']:
        try:
            verify_assets(actual, plan['assets'])
        except ValueError as error:
            raise PublishedReleaseMismatch('Release ' + tag + ' is already published and its attachments '
                                           'differ from this build; it was not modified. ' + str(error)) from error
        return release
    existing = assets_by_name(actual)
    for asset in plan['assets']:
        uploaded = existing.get(asset['name'])
        if uploaded is not None and not asset_matches(uploaded, asset):
            # Rebuilt ZIPs differ byte for byte, so a rerun replaces what an interrupted
            # run left in the unpublished draft instead of failing on it forever.
            client.delete_asset(uploaded)
            uploaded = None
        if uploaded is None:
            client.upload(release, asset_root / asset['name'])
    verify_assets(client.assets(release), plan['assets'])
    client.publish(release)
    release = client.release_by_id(release['id'])
    if release is None or release['draft']:
        raise ValueError('Release was not published.')
    verify_assets(client.assets(release), plan['assets'])
    return release


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    prepare_parser = commands.add_parser('prepare')
    prepare_parser.add_argument('--tag', required=True)
    prepare_parser.add_argument('--expected-sha', required=True)
    commands.add_parser('collect')
    publish_parser = commands.add_parser('publish')
    publish_parser.add_argument('--repo', required=True)
    args = parser.parse_args()
    if args.command == 'prepare':
        prepare(args.tag, args.expected_sha)
    elif args.command == 'collect':
        collect()
    else:
        plan = json.loads(PLAN_PATH.read_text(encoding='utf-8'))
        context = json.loads(CONTEXT_PATH.read_text(encoding='utf-8'))
        notes = ROOT / 'work/release-notes.md'
        notes.write_text(release_notes((ROOT / 'CHANGELOG.md').read_text(encoding='utf-8'),
                                       context['version'], plan['source_commit']), encoding='utf-8')
        try:
            release = publish_plan(GhClient(args.repo), plan, ASSET_ROOT, notes)
        except PublishedReleaseMismatch as error:
            print(error, file=sys.stderr, flush=True)
            raise SystemExit(PUBLISHED_MISMATCH_EXIT_CODE) from error
        write_json(ROOT / 'work/release-published.json', {'url': release['html_url'], 'tag': release['tag_name'], 'draft': release['draft']})
        print(release['html_url'])


if __name__ == '__main__':
    main()
