#!/usr/bin/env python3
"""Catalogue/consumer regression tests; no fake platform acceptance."""
import json
import os
import tempfile
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import acceptance

import run


class CatalogueTests(unittest.TestCase):
    def test_release_linux_targets_supported_packages(self):
        self.assertEqual(run.plan('release-linux', {}), [
            ['nix', 'build', '.#default', '.#portable', '.#container-deps', '--no-link', '--json']])

    def test_every_job_has_real_commands(self):
        values = dict(component='/new/component', package='/nix/package', image='localhost/test',
                      evidence='/data/run')
        for job in run.JOBS:
            with self.subTest(job=job):
                commands = run.plan(job, values)
                self.assertTrue(commands)
                self.assertTrue(all(command and all(isinstance(a, str) and a for a in command) for command in commands))
                self.assertFalse(any(command[:3] == ['cargo', 'xtask', 'ci'] for command in commands))

    def test_missing_install_input_is_blocked(self):
        result = subprocess.run([sys.executable, str(Path(run.__file__)), 'ubuntu-install'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 78)
        self.assertEqual(json.loads(result.stdout)['status'], 'blocked')

    def test_plan_does_not_claim_execution(self):
        result = subprocess.run([sys.executable, str(Path(run.__file__)), 'portable', '--plan'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertFalse(json.loads(result.stdout)['execution'])
        self.assertEqual(json.loads(result.stdout)['commands'], [['cargo', 'xtask', 'check']])


class LabControllerTests(unittest.TestCase):
    """Check orchestration only; these tests do not claim native acceptance."""

    def test_x64_only_products_keep_linux_and_windows_installation_chain(self):
        with tempfile.TemporaryDirectory() as directory:
            vault = Path(directory)
            evidence = vault / 'run'
            evidence.mkdir()
            commit = 'a' * 40
            portable = vault / 'portable'
            manifest = portable / 'runtime/share/vaultsyncd/release-manifest.json'
            manifest.parent.mkdir(parents=True)
            record = {'sourceInfo': {'commit': commit}, 'target': 'x86_64-unknown-linux-gnu'}
            manifest.write_text(json.dumps(record))
            nixos = vault / 'nixos'
            (nixos / 'share/vaultsyncd').mkdir(parents=True)
            (nixos / 'share/vaultsyncd/release-manifest.json').write_text('{}')
            deps = vault / 'vaultsyncd-ubuntu-amd64-dependencies'
            deps.mkdir()
            (evidence / 'products.json').write_text(json.dumps({
                'commit': commit, 'outputs': [{'outputs': {'out': str(path)}}
                                            for path in (portable, nixos, deps)]}))
            component = evidence / 'windows-client'
            (component / 'payload').mkdir(parents=True)
            (component / 'payload/client.zip').write_bytes(b'controller fixture')
            (component / 'receipt.json').write_text(json.dumps({'source': {'commit': commit}}))
            config = vault / 'config.json'
            config.write_text(json.dumps({'evidence': str(evidence)}))
            with patch.object(acceptance, 'ROOT', vault), \
                    patch.object(acceptance, 'EVIDENCE_ROOT', vault), \
                    patch.object(acceptance, 'LAB_LOCK', vault / 'lab.lock'), \
                    patch.dict(os.environ, {'VAULTSYNCD_CI_LAB': '1'}), \
                    patch.object(sys, 'argv', ['acceptance', '--config', str(config), '--phase', 'lab']), \
                    patch.object(acceptance.subprocess, 'check_output', side_effect=[commit, '']), \
                    patch.object(acceptance, 'run') as driver:
                self.assertEqual(acceptance.main(), 0)
                commands = [[str(value) for value in call.args[0]] for call in driver.call_args_list]
                for job in ('nixos-install', 'ubuntu-install', 'windows-cfapi', 'windows-e2e'):
                    self.assertTrue(any(job in command for command in commands), job)
                self.assertTrue(any('tests/platform/windows/s099-installation.py' in command
                                    for command in commands))
                self.assertEqual(len(commands), 9)
                driver.reset_mock()
                # A product for any unsupported Linux target must stop before drivers run.
                manifest.write_text(json.dumps({**record, 'target': 'unsupported-linux-target'}))
                acceptance.subprocess.check_output.side_effect = [commit, '']
                with self.assertRaisesRegex(AssertionError, 'unsupported Linux target'):
                    acceptance.main()
                driver.assert_not_called()


if __name__ == '__main__':
    unittest.main()
