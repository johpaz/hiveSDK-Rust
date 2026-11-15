#!/usr/bin/env python3
"""Offline, conservative Cargo notice inventory. No license branch selection or legal certification."""
import argparse
import configparser
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parent.parent
PREFIXES = ('license', 'licence', 'copying', 'copyright', 'notice')


def sha(data): return hashlib.sha256(data).hexdigest()


def vendored_origin(root, item, vcs, repository, archive):
    """Check a pinned submodule chain; never accept a different commit as a root notice."""
    proof = item['via_submodule']
    if not isinstance(vcs, str) or not re.fullmatch('[0-9a-f]{40}', vcs): raise ValueError('submodule parent requires an exact VCS commit')
    tree_url = f"https://api.github.com/repos/{repository.removeprefix('https://github.com/').removesuffix('.git')}/git/trees/{vcs}?recursive=1"
    if proof['tree_url'] != tree_url: raise ValueError('submodule tree URL must name the parent VCS commit')
    def evidence(digest, suffix):
        if not re.fullmatch('[0-9a-f]{64}', digest): raise ValueError('invalid submodule evidence digest')
        data = (root / 'third_party/upstream' / f'{digest}.{suffix}').read_bytes()
        if sha(data) != digest: raise ValueError('submodule evidence hash mismatch')
        return data
    tree = json.loads(evidence(proof['tree_sha256'], 'json'))
    if tree.get('truncated', True): raise ValueError('submodule tree evidence is truncated')
    entries = {entry['path']: entry for entry in tree['tree']}
    link = entries.get(proof['path'], {})
    if link.get('mode') != '160000' or link.get('type') != 'commit' or link.get('sha') != item['commit']:
        raise ValueError('submodule gitlink does not match notice commit')
    modules = evidence(proof['gitmodules_sha256'], 'txt')
    def blob(data): return hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
    if entries.get('.gitmodules', {}).get('sha') != blob(modules): raise ValueError('submodule configuration does not match tree')
    config = configparser.ConfigParser(interpolation=None); config.read_string(modules.decode('utf-8'))
    repos = [config[section]['url'].removesuffix('.git') for section in config.sections()
             if config[section].get('path') == proof['path']]
    if len(repos) != 1 or not repos[0].startswith('https://github.com/'):
        raise ValueError('submodule repository is missing or ambiguous')
    expected_url = f"https://raw.githubusercontent.com/{repos[0].removeprefix('https://github.com/')}/{item['commit']}/"
    if not re.fullmatch('[0-9a-f]{40}', item['commit']) or not item['url'].startswith(expected_url):
        raise ValueError('submodule notice URL does not match repository and commit')
    # Bind the reviewed vendored source to the checksum-verified crate, not its extracted cache.
    with tarfile.open(archive, 'r:gz') as crate:
        path = proof['source_path']; entry = entries.get(path, {})
        if entry.get('type') != 'blob': raise ValueError('vendored source is missing from parent tree')
        member = crate.getmember(f"{archive.name.removesuffix('.crate')}/{proof['packaged_path']}")
        if not member.isfile() or member.size > 1024 * 1024: raise ValueError('invalid vendored source member')
        if blob(crate.extractfile(member).read()) != entry['sha']: raise ValueError('vendored source does not match parent tree')
    return {'kind': 'pinned_vendored_upstream', 'url': item['url'], 'commit': item['commit'],
            'via_commit': vcs, 'via_submodule': proof}


def lock_packages(data):
    result = {}
    for block in data.decode().split('[[package]]')[1:]:
        fields = {key: json.loads(value) for key, value in re.findall(r'^(name|version|source|checksum) = ("[^\n]*")$', block, re.M)}
        if 'source' in fields: result[(fields['name'], fields['version'], fields['source'])] = fields.get('checksum')
    return result


def collect(root, metadata):
    lock = (root / 'Cargo.lock').read_bytes(); locked = lock_packages(lock)
    supplemental = json.loads((root / 'third_party/upstream-sources.json').read_text())
    if supplemental['schema_version'] != 1: raise ValueError('unsupported supplemental schema')
    rules = {(row['name'], row['version'], row['source']): row for row in supplemental['packages']}
    reviews = supplemental.get('manual_reviews', [])
    packages = []; documents = {}; used_rules = set(); identifiers = set()

    def document(data):
        if not data or len(data) > 1024 * 1024: raise ValueError('notice must contain 1..1 MiB bytes')
        data.decode('utf-8')
        digest = sha(data); documents[digest] = data
        return digest

    for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'], p.get('source') or '')):
        if package['id'] in metadata['workspace_members']: continue
        identity = (package['name'], package['version'], package['source']); identifiers.add(identity)
        if identity not in locked: raise ValueError(f'package not in Cargo.lock: {identity}')
        name, version, source = identity; base = Path(package['manifest_path']).parent; rule = rules.get(identity)
        notices = []; primary = False; vcs = None
        if source.startswith('registry+'):
            archive = base.parents[2] / 'cache' / base.parent.name / f'{name}-{version}.crate'
            checksum = sha(archive.read_bytes())
            if checksum != locked[identity]: raise ValueError(f'crate archive checksum mismatch: {name}@{version}')
            with tarfile.open(archive, 'r:gz') as crate:
                prefix = f'{name}-{version}/'
                for member in sorted(crate.getmembers(), key=lambda entry: entry.name):
                    if not member.name.startswith(prefix): raise ValueError('unexpected archive root')
                    relative = member.name[len(prefix):]; path = PurePosixPath(relative)
                    if path.is_absolute() or '..' in path.parts: raise ValueError('unsafe archive member')
                    if not member.isfile(): continue
                    if relative == '.cargo_vcs_info.json': vcs = json.load(crate.extractfile(member))['git']['sha1']
                    selected = path.name.lower().startswith(PREFIXES + ('authors',)) or relative == package.get('license_file')
                    if not selected: continue
                    if member.size > 1024 * 1024: raise ValueError('notice archive member exceeds 1 MiB')
                    digest = document(crate.extractfile(member).read())
                    notices.append({'sha256': digest, 'origin': {'kind': 'crate_archive', 'path': relative}})
                    primary |= path.name.lower().startswith(PREFIXES) or relative == package.get('license_file') or relative in (rule or {}).get('packaged_evidence', [])
        elif source.startswith('git+'):
            checksum = None; vcs = source.rsplit('#', 1)[-1]
            # Workspace-root Git notices are pinned in the supplemental manifest.
            for file in sorted(base.rglob('*')):
                if file.is_file() and not file.is_symlink() and file.name.lower().startswith(PREFIXES + ('authors',)):
                    relative = file.relative_to(base).as_posix(); digest = document(file.read_bytes())
                    notices.append({'sha256': digest, 'origin': {'kind': 'git_checkout', 'commit': vcs, 'path': relative}})
                    primary |= file.name.lower().startswith(PREFIXES)
        else: raise ValueError(f'unsupported source: {source}')

        if rule is not None:
            used_rules.add(identity)
            if rule.get('checksum_sha256') != checksum: raise ValueError(f'supplemental crate checksum mismatch: {name}@{version}')
            if rule.get('vcs_commit') is not None and rule['vcs_commit'] != vcs: raise ValueError(f'supplemental VCS mismatch: {name}@{version}')
            for item in rule['documents']:
                digest = item['sha256']; data = (root / 'third_party/upstream' / f'{digest}.txt').read_bytes()
                if document(data) != digest: raise ValueError(f'supplemental document hash mismatch: {name}@{version}')
                if 'via_submodule' in item:
                    if not source.startswith('registry+'): raise ValueError('submodule supplements require a verified crate archive')
                    origin = vendored_origin(root, item, vcs, package['repository'], archive)
                else:
                    if item['commit'] != vcs or f'/{vcs}/' not in item['url']: raise ValueError('supplemental URL must name the exact VCS commit')
                    origin = {'kind': 'pinned_upstream', 'url': item['url'], 'commit': vcs}
                notices.append({'sha256': digest, 'origin': origin})
                primary |= 'via_submodule' not in item and PurePosixPath(item['url']).name.lower().startswith(PREFIXES)
        pending = []
        if not primary: pending.append('missing_notice_text')
        if not package.get('license'): pending.append('missing_license_expression')
        for review in reviews:
            if (review['name'], review['version'], review['source']) == identity: pending.append(review['reason'])
        packages.append({'name': name, 'version': version, 'source': source, 'checksum_sha256': checksum,
            'declared_license': package.get('license'), 'repository': package.get('repository'), 'notices': notices, 'pending': pending})
    if set(rules) != used_rules: raise ValueError('stale supplemental entry; update it with Cargo.lock')
    if any((r['name'], r['version'], r['source']) not in identifiers for r in reviews): raise ValueError('stale manual review entry')
    pending = [{'name': p['name'], 'version': p['version'], 'reasons': p['pending']} for p in packages if p['pending']]
    inventory = {'schema_version': 1, 'scope': 'Conservative all-target workspace dependency inventory, including build/dev dependencies; not a list of code linked in an individual binary. License expressions are preserved, not selected or reinterpreted.',
        'cargo_lock_sha256': sha(lock), 'upstream_sources_sha256': sha((root / 'third_party/upstream-sources.json').read_bytes()),
        'status': 'incomplete' if pending else 'notice_sources_collected', 'pending': pending,
        'packages': packages, 'documents': [{'sha256': digest, 'file': f'third_party/notices/{digest}.txt'} for digest in sorted(documents)]}
    lines = ['THIRD-PARTY NOTICES — Hive SDK Rust', '', inventory['scope'],
        'This mechanical inventory is not a legal compliance certification. Pending items are listed below.',
        'Cargo.lock SHA-256: ' + inventory['cargo_lock_sha256'], 'Status: ' + inventory['status'], '', 'PENDING REVIEW']
    lines.extend(f"- {p['name']}@{p['version']}: {', '.join(p['reasons'])}" for p in pending)
    if not pending: lines.append('- No mechanical gaps recorded; this does not certify compliance.')
    lines += ['', 'PACKAGE INDEX']
    for package in packages:
        lines += ['', f"{package['name']}@{package['version']} — {package['declared_license']}", 'Source: ' + package['source']]
        for item in package['notices']:
            origin = item['origin']; label = origin.get('url') or origin['path']
            lines.append(f"  SHA-256 {item['sha256']}: {label}")
    for digest in sorted(documents): lines += ['', '=' * 72, 'DOCUMENT SHA-256 ' + digest, '=' * 72, documents[digest].decode('utf-8')]
    return inventory, documents, ('\n'.join(lines) + '\n').encode('utf-8')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true'); parser.add_argument('--require-complete', action='store_true')
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--offline', '--format-version', '1'], cwd=ROOT))
    inventory, documents, notices = collect(ROOT, metadata)
    inventory['notices_sha256'] = sha(notices)
    outputs = {ROOT / 'third_party/inventory.json': (json.dumps(inventory, indent=2, ensure_ascii=False) + '\n').encode(), ROOT / 'THIRD_PARTY_NOTICES.txt': notices,
        ROOT / 'packages/npm/THIRD_PARTY_NOTICES.txt': notices, ROOT / 'packages/python/THIRD_PARTY_NOTICES.txt': notices, ROOT / 'packages/python/LICENSE': (ROOT / 'LICENSE').read_bytes()}
    inventory_bytes = outputs[ROOT / 'third_party/inventory.json']
    outputs[ROOT / 'packages/npm/THIRD_PARTY_INVENTORY.json'] = inventory_bytes
    outputs[ROOT / 'packages/python/THIRD_PARTY_INVENTORY.json'] = inventory_bytes
    outputs.update({ROOT / 'third_party/notices' / f'{digest}.txt': data for digest, data in documents.items()})
    for path, data in outputs.items():
        if args.check:
            if not path.is_file() or path.read_bytes() != data: raise SystemExit(f'outdated or missing notice output: {path.relative_to(ROOT)}')
        else: path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(data)
    print(f"{len(inventory['packages'])} dependencies, {len(documents)} unique documents, {len(inventory['pending'])} packages pending review; {inventory['status']}")
    if args.require_complete and inventory['pending']:
        for gap in inventory['pending']: print(f"  {gap['name']}@{gap['version']}: {', '.join(gap['reasons'])}")
        raise SystemExit(2)


if __name__ == '__main__': main()
