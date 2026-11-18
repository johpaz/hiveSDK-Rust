"""Notice generation uses verified inputs and fails explicitly for incomplete evidence."""
import contextlib
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('notices', Path(__file__).resolve().parents[1] / 'scripts/third-party-notices.py')
notices = importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(notices)
SOURCE = 'registry+https://github.com/rust-lang/crates.io-index'
COMMIT = 'a' * 40


class NoticeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup); self.root = Path(self.temp.name)
        self.base = self.root / 'registry/src/index/demo-1.0.0'; self.base.mkdir(parents=True)
        self.archive = self.root / 'registry/cache/index/demo-1.0.0.crate'; self.archive.parent.mkdir(parents=True)
        (self.root / 'third_party/upstream').mkdir(parents=True)
        (self.root / 'LICENSE').write_text('SDK license fixture')
        self.rules = {'schema_version': 1, 'packages': []}
        self.metadata = {'workspace_members': ['sdk'], 'packages': [{'id': 'sdk', 'name': 'sdk', 'version': '0.1.0', 'source': None}, {'id': 'dep', 'name': 'demo', 'version': '1.0.0', 'source': SOURCE,
            'manifest_path': str(self.base / 'Cargo.toml'), 'license': 'MIT OR Apache-2.0', 'license_file': None, 'repository': 'https://example.invalid/demo'}]}
        self.write_archive({'LICENSE': b'Permission and copyright fixture', 'vendor/NOTICE': b'Permission and copyright fixture'})

    def write_archive(self, files):
        with tarfile.open(self.archive, 'w:gz') as archive:
            for name, data in files.items():
                info = tarfile.TarInfo('demo-1.0.0/' + name); info.size = len(data); archive.addfile(info, io.BytesIO(data))
        self.checksum = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.root / 'Cargo.lock').write_text(f'version = 4\n\n[[package]]\nname = "demo"\nversion = "1.0.0"\nsource = "{SOURCE}"\nchecksum = "{self.checksum}"\n')
        self.save_rules()

    def save_rules(self):
        (self.root / 'third_party/upstream-sources.json').write_text(json.dumps(self.rules))

    def test_inventory_preserves_expressions_deduplicates_and_has_no_local_paths(self):
        inventory, documents, text = notices.collect(self.root, self.metadata)
        self.assertEqual(inventory['status'], 'notice_sources_collected'); self.assertEqual(len(documents), 1)
        self.assertEqual(inventory['packages'][0]['declared_license'], 'MIT OR Apache-2.0')
        self.assertEqual(len(inventory['packages'][0]['notices']), 2)
        self.assertNotIn(str(self.root), json.dumps(inventory)); self.assertIn(b'not a legal compliance certification', text)

    def test_missing_text_stays_pending_and_completeness_gate_exits_two(self):
        self.write_archive({'src/lib.rs': b'pub fn demo() {}'})
        inventory, _, _ = notices.collect(self.root, self.metadata)
        self.assertEqual(inventory['pending'][0]['reasons'], ['missing_notice_text'])
        with patch.object(notices, 'ROOT', self.root), patch.object(notices.subprocess, 'check_output', return_value=json.dumps(self.metadata).encode()), patch('sys.argv', ['notices', '--require-complete']), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit) as error: notices.main()
        self.assertEqual(error.exception.code, 2)

    def test_changed_archive_and_stale_lock_fail_before_generating_outputs(self):
        self.archive.write_bytes(self.archive.read_bytes() + b'tampered')
        with self.assertRaisesRegex(ValueError, 'archive checksum mismatch'): notices.collect(self.root, self.metadata)
        self.assertFalse((self.root / 'third_party/inventory.json').exists())
        (self.root / 'Cargo.lock').write_text('version = 4\n')
        with self.assertRaisesRegex(ValueError, 'not in Cargo.lock'): notices.collect(self.root, self.metadata)

    def test_supplements_require_matching_crate_commit_and_document_hash(self):
        self.write_archive({'.cargo_vcs_info.json': json.dumps({'git': {'sha1': COMMIT}}).encode()})
        data = b'Upstream copyright and permission fixture'; digest = notices.sha(data)
        path = self.root / f'third_party/upstream/{digest}.txt'; path.write_bytes(data)
        row = {'name': 'demo', 'version': '1.0.0', 'source': SOURCE, 'checksum_sha256': self.checksum, 'vcs_commit': COMMIT,
            'documents': [{'sha256': digest, 'commit': COMMIT, 'url': f'https://raw.githubusercontent.com/test/demo/{COMMIT}/LICENSE'}]}
        self.rules['packages'] = [row]; self.save_rules()
        self.assertEqual(notices.collect(self.root, self.metadata)[0]['pending'], [])
        row['vcs_commit'] = 'b' * 40; self.save_rules()
        with self.assertRaisesRegex(ValueError, 'VCS mismatch'): notices.collect(self.root, self.metadata)
        row['vcs_commit'] = COMMIT; self.save_rules(); path.write_bytes(data + b'tampered')
        with self.assertRaisesRegex(ValueError, 'document hash mismatch'): notices.collect(self.root, self.metadata)

    def test_authors_alone_requires_explicit_evidence_bound_to_archive(self):
        self.write_archive({'AUTHORS': b'Packaged license and copyright fixture'})
        self.assertEqual(notices.collect(self.root, self.metadata)[0]['status'], 'incomplete')
        self.rules['packages'] = [{'name': 'demo', 'version': '1.0.0', 'source': SOURCE, 'checksum_sha256': self.checksum, 'documents': [], 'packaged_evidence': ['AUTHORS']}]; self.save_rules()
        self.assertEqual(notices.collect(self.root, self.metadata)[0]['pending'], [])

    def test_manifest_declaration_counts_as_notice_only_when_named_as_evidence_for_that_archive(self):
        self.write_archive({'Cargo.toml': b'[package]\nlicense = "MIT"\nauthors = ["A Person"]\n', 'README.md': b'readme fixture'})
        self.assertEqual(notices.collect(self.root, self.metadata)[0]['status'], 'incomplete')
        self.rules['packages'] = [{'name': 'demo', 'version': '1.0.0', 'source': SOURCE, 'checksum_sha256': self.checksum, 'documents': [], 'packaged_evidence': ['Cargo.toml']}]; self.save_rules()
        inventory, documents, _ = notices.collect(self.root, self.metadata)
        self.assertEqual(inventory['packages'][0]['pending'], [])
        origins = [n['origin']['path'] for n in inventory['packages'][0]['notices']]
        self.assertEqual(origins, ['Cargo.toml'], 'only the named file is taken, not the README')
        self.assertIn(b'A Person', b''.join(documents.values()))
        self.rules['packages'][0]['checksum_sha256'] = '0' * 64; self.save_rules()
        with self.assertRaises(ValueError): notices.collect(self.root, self.metadata)

    def submodule_fixture(self, parent_notice=True):
        child = 'b' * 40; source = b'// vendored tokenizer fixture'
        modules = b'[submodule "vendor/tokenizer"]\npath = vendor/tokenizer\nurl = https://github.com/test/tokenizer.git\n'
        blob = lambda data: hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
        files = {'.cargo_vcs_info.json': json.dumps({'git': {'sha1': COMMIT}}).encode(), 'src/vendor.rs': source}
        if parent_notice: files['LICENSE'] = b'Wrapper copyright and permission fixture'
        self.write_archive(files)
        tree = {'truncated': False, 'tree': [{'path': '.gitmodules', 'type': 'blob', 'sha': blob(modules)},
            {'path': 'vendor/tokenizer', 'mode': '160000', 'type': 'commit', 'sha': child},
            {'path': 'wrapper/src/vendor.rs', 'type': 'blob', 'sha': blob(source)}]}
        def store(data, suffix):
            digest = notices.sha(data); (self.root / f'third_party/upstream/{digest}.{suffix}').write_bytes(data); return digest
        self.metadata['packages'][1]['repository'] = 'https://github.com/test/wrapper'
        item = {'commit': child, 'url': f'https://raw.githubusercontent.com/test/tokenizer/{child}/LICENSE',
            'sha256': store(b'Upstream vendored permission and copyright fixture', 'txt'), 'via_submodule': {
                'tree_url': f'https://api.github.com/repos/test/wrapper/git/trees/{COMMIT}?recursive=1',
                'tree_sha256': store(json.dumps(tree).encode(), 'json'), 'gitmodules_sha256': store(modules, 'txt'),
                'path': 'vendor/tokenizer', 'source_path': 'wrapper/src/vendor.rs', 'packaged_path': 'src/vendor.rs'}}
        self.rules['packages'] = [{'name': 'demo', 'version': '1.0.0', 'source': SOURCE, 'checksum_sha256': self.checksum,
            'vcs_commit': COMMIT, 'documents': [item]}]; self.save_rules()
        return item

    def test_submodule_notice_preserves_both_commits_and_binds_packaged_source(self):
        self.submodule_fixture()
        inventory, _, _ = notices.collect(self.root, self.metadata)
        self.assertEqual(inventory['pending'], [])
        origin = next(n['origin'] for n in inventory['packages'][0]['notices'] if n['origin']['kind'] == 'pinned_vendored_upstream')
        self.assertEqual(origin['kind'], 'pinned_vendored_upstream')
        self.assertEqual(origin['via_commit'], COMMIT); self.assertEqual(origin['commit'], 'b' * 40)

    def test_submodule_notice_does_not_replace_missing_parent_notice(self):
        self.submodule_fixture(parent_notice=False)
        inventory, _, _ = notices.collect(self.root, self.metadata)
        self.assertEqual(inventory['pending'][0]['reasons'], ['missing_notice_text'])

    def test_submodule_chain_rejects_wrong_parent_child_repository_and_source(self):
        cases = [('parent', 'parent VCS commit'), ('child', 'gitlink'), ('repository', 'repository and commit'),
                 ('source', 'missing from parent tree'), ('packaged', 'does not match parent tree')]
        for case, message in cases:
            with self.subTest(case=case):
                item = self.submodule_fixture(); proof = item['via_submodule']
                if case == 'parent': proof['tree_url'] = proof['tree_url'].replace(COMMIT, 'c' * 40)
                elif case == 'child': item['commit'] = 'c' * 40
                elif case == 'repository': item['url'] = item['url'].replace('test/tokenizer', 'other/tokenizer')
                elif case == 'source': proof['source_path'] = 'other.rs'
                else: proof['packaged_path'] = '.cargo_vcs_info.json'
                self.save_rules()
                with self.assertRaisesRegex(ValueError, message): notices.collect(self.root, self.metadata)

    def test_submodule_evidence_tampering_and_truncated_tree_fail(self):
        item = self.submodule_fixture(); proof = item['via_submodule']
        path = self.root / f"third_party/upstream/{proof['tree_sha256']}.json"
        data = path.read_bytes(); path.write_bytes(data + b' ')
        with self.assertRaisesRegex(ValueError, 'evidence hash mismatch'): notices.collect(self.root, self.metadata)
        tree = json.loads(data); tree['truncated'] = True; data = json.dumps(tree).encode()
        proof['tree_sha256'] = notices.sha(data)
        (self.root / f"third_party/upstream/{proof['tree_sha256']}.json").write_bytes(data); self.save_rules()
        with self.assertRaisesRegex(ValueError, 'truncated'): notices.collect(self.root, self.metadata)

    def test_unsafe_archive_paths_and_stale_supplements_fail(self):
        self.write_archive({'../outside/LICENSE': b'unsafe'})
        with self.assertRaisesRegex(ValueError, 'unsafe archive member'): notices.collect(self.root, self.metadata)
        self.write_archive({'LICENSE': b'fixture'})
        self.rules['packages'] = [{'name': 'other', 'version': '9.0.0', 'source': SOURCE}]; self.save_rules()
        with self.assertRaisesRegex(ValueError, 'stale supplemental'): notices.collect(self.root, self.metadata)

    def test_check_detects_generated_notice_changes_and_lock_drift(self):
        with patch.object(notices, 'ROOT', self.root), patch.object(notices.subprocess, 'check_output', return_value=json.dumps(self.metadata).encode()), contextlib.redirect_stdout(io.StringIO()):
            with patch('sys.argv', ['notices']): notices.main()
            with patch('sys.argv', ['notices', '--check']): notices.main()
            (self.root / 'packages/npm/THIRD_PARTY_NOTICES.txt').write_text('modified')
            with patch('sys.argv', ['notices', '--check']):
                with self.assertRaisesRegex(SystemExit, 'outdated or missing'): notices.main()
            with patch('sys.argv', ['notices']): notices.main()
            with (self.root / 'Cargo.lock').open('a') as file: file.write('# changed\n')
            with patch('sys.argv', ['notices', '--check']):
                with self.assertRaisesRegex(SystemExit, 'outdated or missing'): notices.main()


if __name__ == '__main__': unittest.main()
