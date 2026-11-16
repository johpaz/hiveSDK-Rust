import json
import os
import shutil
import stat
import sys
import tempfile
import unittest
from pathlib import Path
from hive_sdk import NativeTools

FIXTURE = Path(__file__).resolve().parents[3] / 'crates/hive-core/tests/fixtures/fake_obscura.py'


def setup(root):
    executable = Path(root) / 'obscura'
    shutil.copy(FIXTURE, executable)
    executable.chmod(executable.stat().st_mode | stat.S_IEXEC)

    def starts():
        log = Path(f'{executable}.log')
        return sum(1 for line in log.read_text().splitlines() if 'argv' in json.loads(line))
    return str(executable), starts


class ObscuraToolTests(unittest.IsolatedAsyncioTestCase):
    async def test_tools_are_absent_unless_configured_and_sensitive_ones_are_opt_in(self):
        with tempfile.TemporaryDirectory() as root:
            executable, _ = setup(root)

            async def names(config):
                return [t['name'] for t in await NativeTools({'root': root, **config}).definitions() if t['name'].startswith('obscura_')]
            self.assertEqual(await names({}), [])
            base = await names({'obscura': {'executable': executable}})
            self.assertEqual(len(base), 31)
            self.assertNotIn('obscura_evaluate', base)
            self.assertNotIn('obscura_get_cookies', base)
            self.assertIn('obscura_evaluate', await names({'obscura': {'executable': executable, 'allow_evaluate': True}}))
            with self.assertRaisesRegex(Exception, 'timeouts'):
                NativeTools({'root': root, 'obscura': {'call_timeout_ms': 0}})
            async with NativeTools({'root': root, 'obscura': {'executable': executable}}):
                pass

    async def test_install_needs_the_tools_and_a_valid_install_dir(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(Exception, 'disabled'):
                await NativeTools({'root': root}).install_obscura()
            with self.assertRaisesRegex(Exception, 'absolute'):
                NativeTools({'root': root, 'obscura': {'install_dir': 'relative/bin'}})
            NativeTools({'root': root, 'obscura': {'install_dir': root, 'auto_install': False}})
            tools = NativeTools({'root': root, 'obscura': {'executable': '/nonexistent/obscura', 'auto_install': False}})
            with self.assertRaisesRegex(Exception, 'does not exist'):
                await tools.execute('obscura_tab_list', {})

    @unittest.skipIf(sys.platform == 'win32', 'the fake server is a POSIX script')
    async def test_calls_share_one_session_and_close_ends_it(self):
        with tempfile.TemporaryDirectory() as root:
            executable, starts = setup(root)
            async with NativeTools({'root': root, 'obscura': {'executable': executable, 'allowed_hosts': ['example.com']}}) as tools:
                page = await tools.execute('obscura_navigate', {'url': 'https://example.com/a'})
                self.assertEqual(page, {'text': 'URL: https://example.com/a\nTitle: Fake page', 'truncated': False})
                with self.assertRaisesRegex(Exception, 'allowed_hosts'):
                    await tools.execute('obscura_navigate', {'url': 'https://evil.example/'})
                with self.assertRaisesRegex(Exception, 'ended on a blocked address'):
                    await tools.execute('obscura_navigate', {'url': 'https://example.com/redir-bad'})
                shot = await tools.execute('obscura_screenshot', {'width': 32, 'height': 24, 'path': 'shots/p.png', 'create_dirs': True})
                self.assertEqual((shot['width'], shot['height'], shot['path']), (32, 24, 'shots/p.png'))
                self.assertTrue((Path(root) / 'shots/p.png').read_bytes().startswith(b'\x89PNG'))
                self.assertEqual(starts(), 1)
                await tools.close()
                await tools.execute('obscura_tab_list', {})
                self.assertEqual(starts(), 2)
                with self.assertRaisesRegex(Exception, 'TIMEOUT'):
                    await tools.execute('obscura_navigate', {'url': 'https://example.com/hang', 'timeout_ms': 300})
                await tools.execute('obscura_tab_list', {})
                self.assertEqual(starts(), 3)


if __name__ == '__main__':
    unittest.main()
