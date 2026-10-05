"""Collect locked Windows dependency licenses, including MediaWiki table terms."""
import hashlib
import json
import pathlib
import re
import shutil
import subprocess


def same_license_text(left, right):
    # Git may check out license text with CRLF on Windows; wording must still match.
    return left.replace(b'\r\n', b'\n') == right.replace(b'\r\n', b'\n')


root = pathlib.Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output(['cargo','metadata','--format-version','1','--locked','--filter-platform','x86_64-pc-windows-msvc'], cwd=root))
destination = root / 'licenses' / 'rust'
destination.mkdir(parents=True, exist_ok=True)
inventory = []
root_package = metadata['resolve']['root']
zhconv_package = next(package for package in metadata['packages'] if package['name'] == 'zhconv')
zhconv_features = next(node['features'] for node in metadata['resolve']['nodes'] if node['id'] == zhconv_package['id'])
if any('opencc' in feature for feature in zhconv_features):
    raise RuntimeError('OpenCC features must not be enabled in the MediaWiki release.')
if not {'mediawiki-hant', 'mediawiki-tw'}.issubset(zhconv_features):
    raise RuntimeError('Both MediaWiki conversion modes are required.')

for package in sorted(metadata['packages'], key=lambda p:(p['name'],p['version'])):
    if package['id'] == root_package:
        continue
    source = pathlib.Path(package['manifest_path']).parent
    target = destination / (package['name'] + '-' + package['version'])
    found = []
    for file in source.rglob('*'):
        if not file.is_file():
            continue
        name = file.name.upper()
        if not (name.startswith(('LICENSE','LICENCE','COPYING','NOTICE')) or name in ('OFL.TXT','UFL.TXT')):
            continue
        if file.suffix.lower() in ('.rs','.png','.ttf','.otf'):
            continue
        relative = file.relative_to(source)
        output = target / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists():
            if not same_license_text(output.read_bytes(), file.read_bytes()):
                raise RuntimeError('Existing license differs: ' + str(output))
        else:
            shutil.copyfile(file, output)
        found.append(str(output.relative_to(root)).replace('\\','/'))
    inventory.append({'name':package['name'],'version':package['version'],'license':package.get('license'),'files':found})
fallbacks = {
    'clipboard-win':'clipboard-win', 'gl_generator':'gl-rs', 'khronos_api':'gl-rs', 'profiling':'profiling',
    'zune-core':'zune-image','zune-jpeg':'zune-image',
    **{name:'egui' for name in ('ecolor','eframe','egui','egui-winit','egui_glow','emath','epaint')}
}
for package in inventory:
    if not package['files'] and package['name'] in fallbacks:
        directory = root / 'licenses' / 'upstream' / fallbacks[package['name']]
        package['files'] = [str(file.relative_to(root)).replace('\\','/') for file in directory.glob('*') if file.is_file()]

zhconv_source = pathlib.Path(zhconv_package['manifest_path']).parent
mediawiki_source = zhconv_source / 'data' / 'ZhConversion.php'
mediawiki_license = zhconv_source / 'LICENSE-GPL'
mediawiki_destination = root / 'licenses' / 'mediawiki'
mediawiki_destination.mkdir(parents=True, exist_ok=True)
license_output = mediawiki_destination / 'LICENSE-GPL-2.0.txt'
if license_output.exists() and not same_license_text(license_output.read_bytes(), mediawiki_license.read_bytes()):
    raise RuntimeError('Existing MediaWiki license differs; preserve it for review.')
if not license_output.exists():
    shutil.copyfile(mediawiki_license, license_output)
build_source = (zhconv_source / 'build.rs').read_text(encoding='utf-8')
mediawiki_commit = re.search(r'const MEDIAWIKI_COMMIT: &str = "([0-9a-f]{40})";', build_source)
if mediawiki_commit is None:
    raise RuntimeError('MediaWiki provenance is missing from the locked zhconv build script.')
table_sha256 = hashlib.sha256(mediawiki_source.read_bytes()).hexdigest()
provenance = {
    'component': 'MediaWiki Chinese conversion tables',
    'license': 'GPL-2.0-or-later',
    'combined_distribution_license': 'GPL-3.0-only',
    'zhconv_version': zhconv_package['version'],
    'zhconv_features': zhconv_features,
    'mediawiki_commit': mediawiki_commit.group(1),
    'table_path': 'data/ZhConversion.php',
    'table_sha256': table_sha256,
    'source_url': f'https://github.com/wikimedia/mediawiki/blob/{mediawiki_commit.group(1)}/includes/Languages/Data/ZhConversion.php',
}
(mediawiki_destination / 'provenance.json').write_text(json.dumps(provenance, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
zhconv_inventory = next(package for package in inventory if package['name'] == 'zhconv')
zhconv_inventory['license_components'] = {
    'library': 'MIT OR Apache-2.0',
    'mediawiki_tables': 'GPL-2.0-or-later',
}
zhconv_inventory['files'].append('licenses/mediawiki/LICENSE-GPL-2.0.txt')
missing = [package['name'] for package in inventory if not package['files']]
if missing:
    raise RuntimeError('Missing license text for locked dependencies: ' + ', '.join(missing))
(root / 'licenses' / 'rust-inventory.json').write_text(json.dumps(inventory,ensure_ascii=False,indent=2),encoding='utf-8')
print(json.dumps({'packages':len(inventory),'license_files':sum(len(p['files']) for p in inventory),'without_license_files':missing,'mediawiki_table_sha256':table_sha256},ensure_ascii=False))
