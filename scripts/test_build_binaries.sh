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

test_package_linux_zip_creates_archive_with_artifacts() {
  if ! command -v zip >/dev/null 2>&1 || ! command -v unzip >/dev/null 2>&1; then
    echo "skipping linux zip packaging test (zip/unzip not installed)"
    return 0
  fi

  local tmpdir
  tmpdir="$(mktemp -d)"
  local original_linux_dist_dir="$LINUX_DIST_DIR"
  LINUX_DIST_DIR="$tmpdir"

  touch "$LINUX_DIST_DIR/$CLI_APP_NAME"
  touch "$LINUX_DIST_DIR/$GUI_APP_NAME"

  package_linux_zip

  local zip_path="$LINUX_DIST_DIR/ec2_manager_linux.zip"
  if [[ ! -f "$zip_path" ]]; then
    echo "assertion failed: linux zip was not created" >&2
    exit 1
  fi

  local listing
  listing="$(unzip -Z1 "$zip_path" | tr '\n' ' ')"
  if [[ "$listing" != *"$CLI_APP_NAME"* || "$listing" != *"$GUI_APP_NAME"* ]]; then
    echo "assertion failed: linux zip missing expected artifacts" >&2
    echo "  listing: $listing" >&2
    exit 1
  fi

  rm -rf "$tmpdir"
  LINUX_DIST_DIR="$original_linux_dist_dir"
}

test_package_linux_zip_skips_when_no_artifacts() {
  local tmpdir
  tmpdir="$(mktemp -d)"
  local original_linux_dist_dir="$LINUX_DIST_DIR"
  LINUX_DIST_DIR="$tmpdir"

  package_linux_zip

  if [[ -f "$LINUX_DIST_DIR/ec2_manager_linux.zip" ]]; then
    echo "assertion failed: linux zip should not be created without artifacts" >&2
    exit 1
  fi

  rm -rf "$tmpdir"
  LINUX_DIST_DIR="$original_linux_dist_dir"
}

test_copy_windows_runtime_dlls_with_custom_gcc() {
  local tmpdir
  tmpdir="$(mktemp -d)"
  local original_windows_dist_dir="$WINDOWS_DIST_DIR"
  WINDOWS_DIST_DIR="$tmpdir"

  local bin_dir="$tmpdir/bin"
  mkdir -p "$bin_dir"

  local fake_gcc="$bin_dir/fake-gcc"
  cat >"$fake_gcc" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  -print-file-name=libgcc_s_seh-1.dll) echo "$TEST_DLL_DIR/libgcc_s_seh-1.dll" ;;
  -print-file-name=libstdc++-6.dll) echo "$TEST_DLL_DIR/libstdc++-6.dll" ;;
  -print-file-name=libwinpthread-1.dll) echo "$TEST_DLL_DIR/libwinpthread-1.dll" ;;
  *) echo "$1" ;;
esac
EOF
  chmod +x "$fake_gcc"

  local dll_dir="$tmpdir/dlls"
  mkdir -p "$dll_dir"
  touch "$dll_dir/libgcc_s_seh-1.dll"
  touch "$dll_dir/libstdc++-6.dll"
  touch "$dll_dir/libwinpthread-1.dll"

  TEST_DLL_DIR="$dll_dir" EC2_MANAGER_MINGW_GCC="$fake_gcc" copy_windows_runtime_dlls "x86_64-pc-windows-gnu"

  if [[ ! -f "$WINDOWS_DIST_DIR/libgcc_s_seh-1.dll" || ! -f "$WINDOWS_DIST_DIR/libstdc++-6.dll" || ! -f "$WINDOWS_DIST_DIR/libwinpthread-1.dll" ]]; then
    echo "assertion failed: windows runtime DLLs were not copied" >&2
    exit 1
  fi

  rm -rf "$tmpdir"
  WINDOWS_DIST_DIR="$original_windows_dist_dir"
}

main() {
  test_native_mode_uses_host_target_on_linux
  test_windows_mode_target_by_host_os
  test_all_mode_outputs_expected_targets
  test_invalid_mode_fails
  test_package_linux_zip_creates_archive_with_artifacts
  test_package_linux_zip_skips_when_no_artifacts
  test_copy_windows_runtime_dlls_with_custom_gcc
  echo "build_binaries tests passed"
}

main "$@"
