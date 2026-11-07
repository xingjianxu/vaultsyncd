#!/usr/bin/env python3
"""Catalogue/consumer regression tests; no fake platform acceptance."""
import fnmatch
import json
from pathlib import Path
import subprocess
import sys
import unittest

import run


class CatalogueTests(unittest.TestCase):
    def test_workflow_paths_match_catalogue(self):
        workflow = (run.ROOT / '.github/workflows/platform.yml').read_text()
        paths = [json.loads(line.split('paths: ', 1)[1]) for line in workflow.splitlines() if 'paths: [' in line]
        self.assertEqual(paths, [run.PATHS, run.PATHS])

    def test_consumers_and_shared_inputs_trigger(self):
        for path in ['apps/desktop/src-tauri/src/main.rs', 'apps/agent/src/main.rs',
                     'apps/web/src/main.ts', 'crates/protocol/src/generated/mod.rs',
                     'packages/api-client/src/generated/index.ts', 'api/openapi.yaml',
                     'tests/windows-integration/tests/cfapi.rs', 'tests/server-integration/tests/restore.rs',
                     'packaging/windows/installer/vaultsyncd.nsi', 'deploy/compose/run.py',
                     'tools/xtask/src/main.rs', 'nix/package.nix', 'Cargo.lock',
                     'pnpm-workspace.yaml', 'flake.lock', '.cargo/config.toml']:
            with self.subTest(path=path):
                self.assertTrue(any(fnmatch.fnmatchcase(path, p) for p in run.PATHS))

    def test_every_job_has_real_commands(self):
        values = dict(component='/new/component', package='/nix/package', image='localhost/test',
                      vm='/data/run/ubuntu-arm64-tcg', evidence='/data/run')
        for job in run.JOBS:
            with self.subTest(job=job):
                commands = run.plan(job, values)
                self.assertTrue(commands)
                self.assertTrue(all(command and all(isinstance(a, str) and a for a in command) for command in commands))
                self.assertFalse(any(command[:3] == ['cargo', 'xtask', 'ci'] for command in commands))

    def test_missing_install_input_is_blocked(self):
        result = subprocess.run([sys.executable, str(Path(run.__file__)), 'arm64-install'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 78)
        self.assertEqual(json.loads(result.stdout)['status'], 'blocked')

    def test_plan_does_not_claim_execution(self):
        result = subprocess.run([sys.executable, str(Path(run.__file__)), 'portable', '--plan'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertFalse(json.loads(result.stdout)['execution'])
        self.assertEqual(json.loads(result.stdout)['commands'], [['cargo', 'xtask', 'check']])

    def test_lab_is_manual_and_serial(self):
        workflow = (run.ROOT / '.github/workflows/platform-lab.yml').read_text()
        self.assertNotIn('pull_request:', workflow)
        self.assertIn('workflow_dispatch:', workflow)
        self.assertIn('cancel-in-progress: false', workflow)
        self.assertIn('environment: platform-lab', workflow)
        self.assertIn('artifacts/S101/public/', workflow)


if __name__ == '__main__':
    unittest.main()
