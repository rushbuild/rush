#!/usr/bin/env python3
"""Exercise overlapping ownership and rejection of malformed routing input."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('maintenance_check', Path(__file__).with_name('check.py'))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class RoutingTests(unittest.TestCase):
    def test_cross_component_routes_preserve_every_owner(self):
        owners = check.route('crates/rush-build/src/kbuild_snapshot.rs', check.entries())
        self.assertEqual(owners, ['PROJECT INTEGRATION', 'BUILD ORCHESTRATION',
                                  'EXTERNAL CAPTURE AND REPLAY'])

    def test_absolute_and_escaping_paths_are_rejected(self):
        for path in ['/etc/passwd', '../outside', 'crates/../../outside']:
            with self.assertRaises(ValueError):
                check.route(path, check.entries())

    def test_incomplete_owner_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'MAINTAINERS').write_text('BROKEN\nF: **\n')
            with patch.object(check, 'ROOT', root), self.assertRaises(ValueError):
                check.entries()


if __name__ == '__main__':
    unittest.main()
