"""Run the pinned h2spec suite against the repository's loopback fixture."""

import json
from pathlib import Path
import re
import select
import subprocess
import sys
import tempfile
from xml.etree import ElementTree

HOST = "127.0.0.1"
STARTUP_TIMEOUT = 10
SUITE_TIMEOUT = 120
SHUTDOWN_TIMEOUT = 5
# The pinned h2spec 2.6.0 default suite contains 146 cases.
EXPECTED_CASES = 146
# Keep the two dummy fields in the valid continuation probe under 4 KiB.
CONTINUATION_HEADER_LENGTH = 512
CONTINUATION_CASE = "http2/6.10/1"


def build_fixture():
    result = subprocess.run(
        [
            "cargo", "build", "--locked", "-p", "embedded-http2",
            "--example", "h2spec-server", "--message-format=json",
        ],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    for line in result.stdout.splitlines():
        record = json.loads(line)
        if (
            record.get("reason") == "compiler-artifact"
            and record["target"]["name"] == "h2spec-server"
            and record.get("executable")
        ):
            return record["executable"]
    raise RuntimeError("Cargo did not report the h2spec fixture executable")


def run_suite(fixture, report, *options):
    with subprocess.Popen([fixture], text=True, stdout=subprocess.PIPE) as server:
        try:
            if server.stdout is None or not select.select(
                [server.stdout], [], [], STARTUP_TIMEOUT
            )[0]:
                raise RuntimeError("The HTTP/2 fixture did not announce its address")
            host, separator, port = server.stdout.readline().strip().partition(":")
            if host != HOST or not separator or not port.isdecimal():
                raise RuntimeError("The HTTP/2 fixture did not bind to loopback")
            return subprocess.run(
                [
                    "h2spec", "-h", host, "-p", port, "-j", str(report),
                    *options,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                encoding="utf-8",
                errors="replace",
                timeout=SUITE_TIMEOUT,
            )
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=SHUTDOWN_TIMEOUT)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


def check_report(report, exit_code, known_failures, expected_cases=EXPECTED_CASES):
    # h2spec can include raw peer bytes in XML error text. XML 1.0 excludes
    # these control characters even in text nodes.
    xml = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f]", "", report.read_text(errors="replace"))
    suites = ElementTree.fromstring(xml)
    cases = {}
    failed = set()
    for suite in suites.iter("testsuite"):
        for index, case in enumerate(suite.findall("testcase"), start=1):
            case_id = f"{suite.attrib['package']}/{index}"
            if case_id in cases or case.find("skipped") is not None:
                raise RuntimeError(f"Duplicate or skipped h2spec case {case_id}")
            cases[case_id] = case.attrib["classname"]
            if case.find("failure") is not None or case.find("error") is not None:
                failed.add(case_id)

    if len(cases) != expected_cases:
        raise RuntimeError(f"h2spec ran {len(cases)} of {expected_cases} expected cases")
    if exit_code != int(bool(failed)):
        raise RuntimeError(f"h2spec exited with {exit_code}, inconsistent with its report")
    unknown = known_failures - cases.keys()
    if unknown:
        raise RuntimeError(f"Unknown cases in the h2spec baseline: {', '.join(sorted(unknown))}")

    print(f"h2spec: {len(cases) - len(failed)} passed, {len(failed)} failed")
    for case_id in sorted(known_failures - failed):
        print(f"Baseline case now passing: {case_id} {cases[case_id]}")
    for case_id in sorted(failed - known_failures):
        print(f"Unexpected failure: {case_id} {cases[case_id]}", file=sys.stderr)
    return not (failed - known_failures)


def main():
    known_failures = {
        line.strip()
        for line in Path(__file__).with_name("known-failures.txt").read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }
    with tempfile.TemporaryDirectory(prefix="http2-conformance-") as directory:
        fixture = build_fixture()
        report = Path(directory) / "results.xml"
        result = run_suite(fixture, report)
        try:
            passed = check_report(report, result.returncode, known_failures)
        except (OSError, RuntimeError, KeyError, ElementTree.ParseError) as error:
            print(error, file=sys.stderr)
            passed = False
        if not passed:
            print(result.stdout, file=sys.stderr)
            return 1
        continuation = Path(directory) / "continuation.xml"
        result = run_suite(
            fixture, continuation,
            "--max-header-length", str(CONTINUATION_HEADER_LENGTH), CONTINUATION_CASE,
        )
        if not check_report(continuation, result.returncode, set(), expected_cases=1):
            print(result.stdout, file=sys.stderr)
            return 1
        print(f"{CONTINUATION_CASE} passed within the advertised header limit")
        return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (
        OSError, RuntimeError, KeyError, ElementTree.ParseError, subprocess.SubprocessError
    ) as error:
        print(f"HTTP/2 conformance run failed: {error}", file=sys.stderr)
        sys.exit(1)
