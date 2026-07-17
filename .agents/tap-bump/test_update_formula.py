#!/usr/bin/env python3

import tempfile
import unittest
from pathlib import Path

from update_formula import update_formula


FORMULA = '''class Hearth < Formula
  desc "Unified Laravel development command center"
  version "0.3.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Naoray/hearth/releases/download/v0.3.0/hearth-v0.3.0-aarch64-apple-darwin.tar.gz"
      sha256 "2181670a660d8b99c317c1bd6230acdd69c3c114775a9e2c438ea4b9ed8af863"
    else
      url "https://github.com/Naoray/hearth/releases/download/v0.3.0/hearth-v0.3.0-x86_64-apple-darwin.tar.gz"
      sha256 "c405904a3fd3676ce015045a4027c3d4125c31ef6e1749c28b93bf97d916de74"
    end
  end
end
'''

ARM_SHA = "a" * 64
X86_SHA = "b" * 64


class UpdateFormulaTest(unittest.TestCase):
    def write_formula(self, content: str = FORMULA) -> Path:
        directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        path = directory / "hearth.rb"
        path.write_text(content, encoding="utf-8")
        return path

    def test_updates_version_urls_and_both_architecture_hashes(self) -> None:
        path = self.write_formula()

        update_formula(path, "v1.2.3-rc.1", ARM_SHA, X86_SHA)

        updated = path.read_text(encoding="utf-8")
        self.assertIn('version "1.2.3-rc.1"', updated)
        self.assertIn(
            "releases/download/v1.2.3-rc.1/"
            "hearth-v1.2.3-rc.1-aarch64-apple-darwin.tar.gz",
            updated,
        )
        self.assertIn(
            "releases/download/v1.2.3-rc.1/"
            "hearth-v1.2.3-rc.1-x86_64-apple-darwin.tar.gz",
            updated,
        )
        self.assertEqual(updated.count(f'sha256 "{ARM_SHA}"'), 1)
        self.assertEqual(updated.count(f'sha256 "{X86_SHA}"'), 1)

    def test_refuses_incomplete_formula_without_writing_partial_changes(self) -> None:
        incomplete = FORMULA.replace(
            '      url "https://github.com/Naoray/hearth/releases/download/v0.3.0/hearth-v0.3.0-x86_64-apple-darwin.tar.gz"\n'
            '      sha256 "c405904a3fd3676ce015045a4027c3d4125c31ef6e1749c28b93bf97d916de74"\n',
            "",
        )
        path = self.write_formula(incomplete)

        with self.assertRaisesRegex(ValueError, "x86_64-apple-darwin"):
            update_formula(path, "v1.2.3", ARM_SHA, X86_SHA)

        self.assertEqual(path.read_text(encoding="utf-8"), incomplete)


if __name__ == "__main__":
    unittest.main()
