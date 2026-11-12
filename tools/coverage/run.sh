usage() {
  cat <<'HELP'
Usage: nix run .#coverage -- [--source PATH]... [-- CARGO_TEST_ARGS...]

Run instrumented host tests and generate terminal and HTML coverage reports.
Repeat --source to limit the report to source files or directories.
Defaults to the workspace's normal host packages and sources under crates/ and tools/.
Set COVERAGE_BUILD_DIR to change the build and report directory.
HELP
}

sources=()
while (($#)); do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    --source)
      if (($# < 2)) || [[ ! -e "$2" ]]; then
        printf '%s\n' '--source requires an existing file or directory.' >&2
        exit 2
      fi
      sources+=("$2")
      shift 2
      ;;
    --)
      shift
      break
      ;;
    *)
      printf 'Unknown coverage option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ ! -f Cargo.toml || ! -d crates || ! -d tools ]]; then
  printf '%s\n' 'Run coverage from the repository root.' >&2
  exit 2
fi

if ((${#sources[@]} == 0)); then
  sources=(crates tools)
fi
if (($# == 0)); then
  set -- --lib --tests
fi

coverage_build="${COVERAGE_BUILD_DIR:-target/coverage-build}"
mkdir -p "$coverage_build"
export CARGO_TARGET_DIR
CARGO_TARGET_DIR="$(cd "$coverage_build" && pwd)"
coverage_run="$(mktemp -d "$CARGO_TARGET_DIR/report.XXXXXX")"
printf 'Coverage run at %s\n' "$coverage_run"

export LLVM_PROFILE_FILE="$coverage_run/%p-%m.profraw"
export RUSTFLAGS="-C instrument-coverage"
export CARGO_INCREMENTAL=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_TEST_OPT_LEVEL=0

if ! cargo test --locked --message-format=json "$@" > "$coverage_run/cargo.json"; then
  printf 'See %s/cargo.json for test output.\n' "$coverage_run" >&2
  exit 1
fi

jq -Rr '
  fromjson?
  | select(.reason == "compiler-artifact" and .profile.test and .executable)
  | .executable
' "$coverage_run/cargo.json" > "$coverage_run/binaries"

objects=()
while IFS= read -r binary; do
  objects+=(-object "$binary")
done < "$coverage_run/binaries"
shopt -s nullglob
profiles=("$coverage_run"/*.profraw)

if ((${#objects[@]} == 0 || ${#profiles[@]} == 0)); then
  printf '%s\n' 'No test executables or coverage profiles found.' >&2
  exit 1
fi

profile="$coverage_run/coverage.profdata"
llvm-profdata merge -sparse "${profiles[@]}" -o "$profile"
llvm-cov report "${objects[@]}" -instr-profile="$profile" --sources "${sources[@]}"
llvm-cov show "${objects[@]}" -instr-profile="$profile" -format=html \
  -output-dir="$coverage_run/html" --sources "${sources[@]}"
printf 'HTML report at %s/html/index.html\n' "$coverage_run"
