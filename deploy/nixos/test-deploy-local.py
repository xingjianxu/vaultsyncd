"""部署控制面的失败关闭契约；不激活 NixOS 或接触真实状态。"""
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('deploy_local', Path(__file__).with_name('deploy-local.py'))
deploy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deploy)


class PinTests(unittest.TestCase):
    old = '1' * 40
    new = '2' * 40
    source = 'other = { rev = "' + old + '"; }; vaultsync = { enable = true; rev = "' + old + '"; port = 8721; };'

    def test_only_service_pin_changes(self):
        result = deploy.pin(self.source, self.new)
        self.assertIn('other = { rev = "' + self.old + '";', result)
        self.assertIn('rev = "' + self.new + '"; port = 8721;', result)

    def test_ambiguous_or_disabled_pin_is_rejected(self):
        for source in [self.source * 2, self.source.replace('true', 'false'), 'vaultsync.rev = "";']:
            with self.subTest(source=source), self.assertRaises(ValueError):
                deploy.pin(source, self.new)

    def test_dirty_workspace_is_rejected(self):
        with patch.object(deploy, 'git', return_value=' M hosts/wks-lab/default.nix'):
            with self.assertRaises(RuntimeError):
                deploy.clean('/unused')

    def test_command_failure_is_propagated(self):
        with self.assertRaises(subprocess.CalledProcessError):
            deploy.command(['false'])

    def test_unhealthy_server_never_passes(self):
        cfg = dict(domain=None, port=1)
        with patch.object(deploy.urllib.request, 'urlopen', side_effect=OSError('offline')), \
             patch.object(deploy.time, 'sleep'), patch.object(deploy, 'command') as command:
            with self.assertRaises(RuntimeError):
                deploy.health(cfg)
            command.assert_not_called()

    def test_server_identity_change_is_rejected(self):
        import io
        cfg = dict(domain=None, port=1)
        def request(url, **kwargs):
            return io.StringIO('{"status":"ok"}' if url.endswith('/health') else '{"server_id":"unexpected"}')
        with patch.object(deploy.urllib.request, 'urlopen', side_effect=request), \
             patch.object(deploy.time, 'sleep'), patch.object(deploy, 'command') as command:
            with self.assertRaises(RuntimeError):
                deploy.health(cfg, previous_id='original', verify_runtime=False)
            command.assert_not_called()


if __name__ == '__main__':
    unittest.main()
