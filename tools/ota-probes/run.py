"""Run the verify-only OTA probes on a dedicated development board."""

import argparse
import ipaddress
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("board_ip", type=ipaddress.IPv4Address)
    parser.add_argument("image", type=Path)
    parser.add_argument("version")
    args = parser.parse_args()
    if not args.image.is_file():
        parser.error(f"image does not exist: {args.image}")

    root = Path(__file__).resolve().parents[2]
    probes = (
        ("live Logs", "grpc-concurrent", False),
        ("stalled Logs", "grpc-concurrent", True),
        ("cancellation cleanup", "grpc-cleanup", False),
        ("firmware parity", "grpc-parity", False),
    )
    failures = []
    for name, example, stalled in probes:
        env = os.environ.copy()
        env.pop("GRPC_STALL_LOGS", None)
        if stalled:
            env["GRPC_STALL_LOGS"] = "1"
        print(f"Running {name}", flush=True)
        try:
            subprocess.run(
                [
                    "cargo", "run", "--locked", "-p", "espctl", "--example",
                    example, "--", str(args.board_ip), str(args.image.resolve()),
                    args.version,
                ],
                cwd=root,
                env=env,
                check=True,
                timeout=240,
            )
        except (OSError, subprocess.SubprocessError) as error:
            print(f"FAIL {name}: {error}", flush=True)
            failures.append(name)

    print(f"OTA probes passed: {len(probes) - len(failures)}/{len(probes)}")
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
