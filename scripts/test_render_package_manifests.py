"""Tests for scripts/render-package-manifests.py (run: python -m unittest discover -s scripts)."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "render-package-manifests.py"
spec = importlib.util.spec_from_file_location("render_package_manifests", SCRIPT)
assert spec is not None and spec.loader is not None
render = importlib.util.module_from_spec(spec)
spec.loader.exec_module(render)

# Independent worked example: one distinct digit per target (see SUMS).
SUMS = (
    f"{'1' * 64}  zpane-v1.2.3-aarch64-apple-darwin.tar.gz\n"
    f"{'2' * 64}  zpane-v1.2.3-x86_64-apple-darwin.tar.gz\n"
    f"{'3' * 64}  zpane-v1.2.3-aarch64-unknown-linux-musl.tar.gz\n"
    f"{'4' * 64}  zpane-v1.2.3-x86_64-unknown-linux-musl.tar.gz\n"
    f"{'5' * 64}  zpane-v1.2.3-x86_64-pc-windows-msvc.zip\n"
    f"{'a' * 64}  zpane_1.2.3-1_amd64.deb\n"
)


class RenderPackageManifestsTest(unittest.TestCase):
    def run_main(self, out: Path, *extra: str, sums: str = SUMS, version: str = "v1.2.3") -> int:
        sums_path = out / "SHA256SUMS"
        sums_path.parent.mkdir(parents=True, exist_ok=True)
        sums_path.write_text(sums, encoding="utf-8")
        return render.main(["--version", version, "--sums", str(sums_path), "--out", str(out / "rendered"), *extra])

    def test_parse_sums_accepts_text_and_binary_markers(self) -> None:
        parsed = render.parse_sums(f"{'A' * 64}  one.zip\n{'b' * 64} *two.tar.gz\n")
        self.assertEqual({"one.zip": "a" * 64, "two.tar.gz": "b" * 64}, parsed)

    def test_parse_sums_rejects_malformed_lines(self) -> None:
        with self.assertRaises(ValueError):
            render.parse_sums("not-a-checksum  file.zip\n")

    def test_renders_every_manifest_with_release_hashes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            self.assertEqual(0, self.run_main(out))
            rendered = out / "rendered"

            formula = (rendered / "homebrew/zpane.rb").read_text(encoding="utf-8")
            self.assertIn('version "1.2.3"', formula)
            self.assertIn(
                "releases/download/v1.2.3/zpane-v1.2.3-aarch64-apple-darwin.tar.gz", formula
            )
            self.assertIn(f'sha256 "{"1" * 64}"', formula)
            self.assertIn(f'sha256 "{"2" * 64}"', formula)
            self.assertIn(f'sha256 "{"3" * 64}"', formula)
            self.assertIn(f'sha256 "{"4" * 64}"', formula)

            scoop = json.loads((rendered / "scoop/zpane.json").read_text(encoding="utf-8"))
            self.assertEqual("1.2.3", scoop["version"])
            self.assertEqual("5" * 64, scoop["architecture"]["64bit"]["hash"])
            self.assertTrue(
                scoop["architecture"]["64bit"]["url"].endswith("/v1.2.3/zpane-v1.2.3-x86_64-pc-windows-msvc.zip")
            )

            installer = (rendered / "winget/zenthora.zpane.installer.yaml").read_text(encoding="utf-8")
            self.assertIn("PackageVersion: 1.2.3", installer)
            self.assertIn(f"InstallerSha256: {'5' * 64}", installer)
            self.assertNotIn("__", installer)

    def test_missing_archive_checksum_fails_without_writing_manifests(self) -> None:
        sums_without_windows = "\n".join(line for line in SUMS.splitlines() if "windows" not in line) + "\n"
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            self.assertEqual(1, self.run_main(out, sums=sums_without_windows))
            self.assertFalse((out / "rendered").exists())

    def test_invalid_version_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(1, self.run_main(Path(tmp), version="1.2"))

    def test_source_sha256_pins_termux_and_arch_checksums(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            self.assertEqual(0, self.run_main(out, "--source-sha256", "AB" * 32))
            rendered = out / "rendered"
            termux = (rendered / "termux/build.sh").read_text(encoding="utf-8")
            pkgbuild = (rendered / "arch/PKGBUILD").read_text(encoding="utf-8")
            self.assertIn(f"TERMUX_PKG_SHA256={'ab' * 32}\n", termux)
            self.assertNotIn("SKIP_CHECKSUM", termux)
            self.assertIn(f"sha256sums=('{'ab' * 32}')\n", pkgbuild)

    def test_archive_names_match_release_asset_naming(self) -> None:
        self.assertEqual(
            "zpane-v1.2.3-x86_64-pc-windows-msvc.zip",
            render.archive_name("1.2.3", "x86_64-pc-windows-msvc"),
        )
        self.assertEqual(
            "zpane-v1.2.3-aarch64-apple-darwin.tar.gz",
            render.archive_name("1.2.3", "aarch64-apple-darwin"),
        )


if __name__ == "__main__":
    unittest.main()
