"""Exercise the firmware config build through Cargo with disposable YAML profiles."""

import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest


REPO = Path(__file__).resolve().parents[3]
PROFILE = """\
wifi:
  ssid: 'test "network"'
  password: 'synthetic-config-test-secret'
firmware:
  version: "first-build"
telemetry:
  imu:
    mqtt:
      broker: "192.0.2.10:1883"
      device_id: "esp32-01"
  soil:
    udp_host: "192.0.2.11"
diagnostics:
  ota_heap_reserve_bytes: 4096
"""


class FirmwareConfig(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.profile = Path(directory.name) / "firmware.local.yml"

    def build_version(self):
        return subprocess.run(
            ["cargo", "run", "--locked", "--quiet", "-p", "firmware-config",
             "--bin", "firmware-version"],
            cwd=REPO,
            env={**os.environ, "FIRMWARE_CONFIG": str(self.profile)},
            capture_output=True,
            text=True,
            timeout=180,
        )

    def test_edit_rebuilds_selected_profile(self):
        for version in ("first-build", "second-build"):
            with self.subTest(version=version):
                self.profile.write_text(PROFILE.replace("first-build", version))
                result = self.build_version()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), version)

    def test_missing_profile_explains_setup(self):
        result = self.build_version()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Copy firmware.example.yaml to firmware.yaml", result.stderr)

    def test_invalid_profiles_fail_without_echoing_credentials(self):
        cases = (
            ("password:", "typo:", "invalid firmware settings"),
            ("  imu:\n", '  imu:\n    udp_host: "192.0.2.10"\n', "invalid firmware settings"),
            ('"first-build"', '"../invalid"', "firmware.version must be"),
            (":1883", ":0", "nonzero TCP port"),
            ('"esp32-01"', '"esp32/#"', "telemetry.imu.mqtt.device_id must be"),
            ("192.0.2.11", "localhost", "invalid firmware settings"),
        )
        for original, replacement, error in cases:
            with self.subTest(replacement=replacement):
                self.profile.write_text(PROFILE.replace(original, replacement))
                result = self.build_version()
                self.assertNotIn("synthetic-config-test-secret", result.stderr)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(error, result.stderr)


if __name__ == "__main__":
    unittest.main()
