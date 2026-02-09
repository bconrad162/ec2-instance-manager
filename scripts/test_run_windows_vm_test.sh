#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${ROOT_DIR}/scripts/run_windows_vm_test.sh"

if [[ ! -x "$SCRIPT" ]]; then
  echo "assertion failed: script should be executable: $SCRIPT" >&2
  exit 1
fi

help_text="$("$SCRIPT" --help)"

if [[ "$help_text" != *"--skip-build"* ]]; then
  echo "assertion failed: help output should mention --skip-build" >&2
  exit 1
fi

if [[ "$help_text" != *"--wait-seconds"* ]]; then
  echo "assertion failed: help output should mention --wait-seconds" >&2
  exit 1
fi

if [[ "$help_text" != *"--skip-gui-terminal-tests"* ]]; then
  echo "assertion failed: help output should mention --skip-gui-terminal-tests" >&2
  exit 1
fi

echo "run_windows_vm_test helper script tests passed"
