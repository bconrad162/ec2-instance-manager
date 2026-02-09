#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run() {
  echo "==> shell syntax checks"
  bash -n "$ROOT_DIR/scripts/build_binaries.sh"
  bash -n "$ROOT_DIR/scripts/run_and_test.sh"
  bash -n "$ROOT_DIR/scripts/test_build_binaries.sh"

  echo "==> build script unit tests"
  bash "$ROOT_DIR/scripts/test_build_binaries.sh"

  echo "==> rust formatting"
  (cd "$ROOT_DIR" && cargo fmt --check)

  echo "==> unit tests"
  (cd "$ROOT_DIR" && cargo test)

  echo "==> gui compile/tests"
  (cd "$ROOT_DIR" && cargo test --features gui --bin ec2_manager_gui)

  echo "==> sim smoke: list terminals"
  (cd "$ROOT_DIR" && cargo run -- --mode sim --list-terminals)

  echo "==> sim smoke: connect dry-run"
  (
    cd "$ROOT_DIR" && \
    cargo run -- --mode sim --search prod --only-ssm --connect i-sim0001 --dry-run
  )

  echo "==> sim smoke: port-forward dry-run"
  (
    cd "$ROOT_DIR" && \
    cargo run -- --mode sim --port-forward i-sim0001 --local-port 15432 --remote-port 5432 --dry-run
  )

  echo "==> sim smoke: interactive shell"
  (
    cd "$ROOT_DIR" && \
    printf 'help\nquit\n' | cargo run -- --mode sim --interactive --dry-run
  )

  echo "==> gui smoke: --help"
  (cd "$ROOT_DIR" && cargo run --features gui --bin ec2_manager_gui -- --help)

  echo "==> all checks passed"
}

run "$@"
