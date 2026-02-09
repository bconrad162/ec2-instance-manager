#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

EC2_MANAGER_BUILD_LIB_ONLY=1 source "$ROOT_DIR/scripts/build_binaries.sh"

assert_eq() {
  local got="$1"
  local want="$2"
  local msg="$3"
  if [[ "$got" != "$want" ]]; then
    echo "assertion failed: $msg" >&2
    echo "  got:  $got" >&2
    echo "  want: $want" >&2
    exit 1
  fi
}

test_native_mode_uses_host_target_on_linux() {
  local got
  got="$(resolve_targets native Linux aarch64-unknown-linux-gnu)"
  assert_eq "$got" "aarch64-unknown-linux-gnu" "native mode should use host target on Linux"
}

test_windows_mode_target_by_host_os() {
  local got_linux got_darwin
  got_linux="$(resolve_targets windows Linux x86_64-unknown-linux-gnu)"
  got_darwin="$(resolve_targets windows Darwin aarch64-apple-darwin)"
  assert_eq "$got_linux" "x86_64-pc-windows-gnu" "Linux should use Windows GNU target"
  assert_eq "$got_darwin" "x86_64-pc-windows-msvc" "non-Linux should use Windows MSVC target"
}

test_all_mode_outputs_expected_targets() {
  local linux_targets darwin_targets
  linux_targets="$(resolve_targets all Linux x86_64-unknown-linux-gnu | tr '\n' ' ' | sed 's/ $//')"
  darwin_targets="$(resolve_targets all Darwin aarch64-apple-darwin)"
  assert_eq "$linux_targets" "x86_64-unknown-linux-gnu x86_64-pc-windows-gnu" "all mode on Linux should emit two targets"
  assert_eq "$darwin_targets" "aarch64-apple-darwin" "all mode on non-Linux should use host target"
}

test_invalid_mode_fails() {
  if resolve_targets nope Linux x86_64-unknown-linux-gnu >/dev/null 2>&1; then
    echo "assertion failed: invalid mode should fail" >&2
    exit 1
  fi
}

main() {
  test_native_mode_uses_host_target_on_linux
  test_windows_mode_target_by_host_os
  test_all_mode_outputs_expected_targets
  test_invalid_mode_fails
  echo "build_binaries tests passed"
}

main "$@"
