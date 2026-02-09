#!/usr/bin/env bash
set -euo pipefail

CLI_APP_NAME="ec2_manager"
GUI_APP_NAME="ec2_manager_gui"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
LINUX_DIST_DIR="${DIST_DIR}/linux"
WINDOWS_DIST_DIR="${DIST_DIR}/windows"
HOST_TRIPLE="$(rustc -vV | awk '/host:/ {print $2}')"

usage() {
  cat <<USAGE
Usage: $0 [native|all|linux|windows]

Build release binaries for Pop!_OS (linux) and Windows.

Output layout:
  dist/linux/
  dist/windows/

Modes:
  native   Build only host-native binaries
  all      Build linux + windows binaries (best on Linux host)
  linux    Build Linux binaries
  windows  Build Windows binaries
USAGE
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: required command not found: $cmd" >&2
    exit 1
  fi
}

ensure_target() {
  local target="$1"

  if rustup target list --installed | grep -qx "$target"; then
    return 0
  fi

  echo "info: installing Rust target: $target"
  rustup target add "$target"
}

target_output_dir() {
  local target="$1"
  if [[ "$target" == *"windows"* ]]; then
    echo "$WINDOWS_DIST_DIR"
  else
    echo "$LINUX_DIST_DIR"
  fi
}

copy_artifact() {
  local target="$1"
  local artifact_name="$2"

  local source_dir
  if [[ "$target" == "$HOST_TRIPLE" ]]; then
    source_dir="$ROOT_DIR/target/release"
  else
    source_dir="$ROOT_DIR/target/${target}/release"
  fi

  local out_dir
  out_dir="$(target_output_dir "$target")"
  mkdir -p "$out_dir"

  cp "${source_dir}/${artifact_name}" "${out_dir}/${artifact_name}"
}

copy_windows_runtime_dlls() {
  local target="$1"

  if [[ "$target" != "x86_64-pc-windows-gnu" ]]; then
    return 0
  fi

  local dll_names=(
    "libgcc_s_seh-1.dll"
    "libstdc++-6.dll"
    "libwinpthread-1.dll"
  )

  for dll in "${dll_names[@]}"; do
    local dll_path
    dll_path="$(x86_64-w64-mingw32-gcc -print-file-name="$dll")"

    if [[ -z "$dll_path" || "$dll_path" == "$dll" || ! -f "$dll_path" ]]; then
      echo "error: unable to locate required Windows runtime DLL: $dll" >&2
      echo "hint: ensure mingw-w64 is fully installed (sudo apt-get install -y mingw-w64)" >&2
      exit 1
    fi

    cp "$dll_path" "$WINDOWS_DIST_DIR/$dll"
  done
}

package_windows_zip() {
  local zip_path="${WINDOWS_DIST_DIR}/ec2_manager_windows.zip"
  local candidates=(
    "${WINDOWS_DIST_DIR}/${CLI_APP_NAME}.exe"
    "${WINDOWS_DIST_DIR}/${GUI_APP_NAME}.exe"
    "${WINDOWS_DIST_DIR}/libgcc_s_seh-1.dll"
    "${WINDOWS_DIST_DIR}/libstdc++-6.dll"
    "${WINDOWS_DIST_DIR}/libwinpthread-1.dll"
  )
  local files=()

  for candidate in "${candidates[@]}"; do
    if [[ -f "$candidate" ]]; then
      files+=("$candidate")
    fi
  done

  if [[ "${#files[@]}" -eq 0 ]]; then
    echo "warning: no Windows artifacts found to package; skipping zip creation"
    return 0
  fi

  require_cmd zip
  rm -f "$zip_path"
  zip -q -j "$zip_path" "${files[@]}"
  echo "info: packaged Windows zip: $zip_path"
}

package_linux_zip() {
  local zip_path="${LINUX_DIST_DIR}/ec2_manager_linux.zip"
  local candidates=(
    "${LINUX_DIST_DIR}/${CLI_APP_NAME}"
    "${LINUX_DIST_DIR}/${GUI_APP_NAME}"
  )
  local files=()

  for candidate in "${candidates[@]}"; do
    if [[ -f "$candidate" ]]; then
      files+=("$candidate")
    fi
  done

  if [[ "${#files[@]}" -eq 0 ]]; then
    echo "warning: no Linux artifacts found to package; skipping zip creation"
    return 0
  fi

  require_cmd zip
  rm -f "$zip_path"
  zip -q -j "$zip_path" "${files[@]}"
  echo "info: packaged Linux zip: $zip_path"
}

build_for_target() {
  local target="$1"

  echo "info: building release target: $target"

  if [[ "$target" == "$HOST_TRIPLE" ]]; then
    (cd "$ROOT_DIR" && cargo build --release --bin "$CLI_APP_NAME")
    copy_artifact "$target" "$CLI_APP_NAME"

    (cd "$ROOT_DIR" && cargo build --release --features gui --bin "$GUI_APP_NAME")
    copy_artifact "$target" "$GUI_APP_NAME"
    if [[ "$target" == *"windows"* ]]; then
      package_windows_zip
    else
      package_linux_zip
    fi
    return 0
  fi

  ensure_target "$target"

  if [[ "$target" == "x86_64-pc-windows-gnu" ]]; then
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
      echo "error: missing cross-linker x86_64-w64-mingw32-gcc for Windows GNU target." >&2
      echo "hint: on Pop!_OS install: sudo apt-get install -y mingw-w64" >&2
      exit 1
    fi
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
  fi

  (cd "$ROOT_DIR" && cargo build --release --target "$target" --bin "$CLI_APP_NAME")
  if [[ "$target" == *"windows"* ]]; then
    copy_artifact "$target" "${CLI_APP_NAME}.exe"
  else
    copy_artifact "$target" "$CLI_APP_NAME"
  fi

  (cd "$ROOT_DIR" && cargo build --release --target "$target" --features gui --bin "$GUI_APP_NAME")
  if [[ "$target" == *"windows"* ]]; then
    copy_artifact "$target" "${GUI_APP_NAME}.exe"
    copy_windows_runtime_dlls "$target"
    package_windows_zip
  else
    copy_artifact "$target" "$GUI_APP_NAME"
    package_linux_zip
  fi
}

resolve_targets() {
  local mode="$1"
  local os="$2"
  local host_triple="$3"

  case "$mode" in
    native)
      echo "$host_triple"
      ;;
    linux)
      echo "x86_64-unknown-linux-gnu"
      ;;
    windows)
      if [[ "$os" == "Linux" ]]; then
        echo "x86_64-pc-windows-gnu"
      else
        echo "x86_64-pc-windows-msvc"
      fi
      ;;
    all)
      if [[ "$os" == "Linux" ]]; then
        echo "x86_64-unknown-linux-gnu"
        echo "x86_64-pc-windows-gnu"
      else
        echo "$host_triple"
      fi
      ;;
    *)
      return 1
      ;;
  esac
}

main() {
  require_cmd cargo
  require_cmd rustup
  require_cmd rustc

  local mode="${1:-all}"
  case "$mode" in
    -h|--help)
      usage
      exit 0
      ;;
  esac

  mkdir -p "$LINUX_DIST_DIR" "$WINDOWS_DIST_DIR"

  local os
  os="$(uname -s)"

  if [[ "$mode" == "all" && "$os" != "Linux" ]]; then
    echo "warning: on non-Linux hosts, full linux+windows cross-build may require extra toolchains."
  fi

  local targets=()
  if ! mapfile -t targets < <(resolve_targets "$mode" "$os" "$HOST_TRIPLE"); then
    echo "error: invalid mode: $mode" >&2
    usage
    exit 1
  fi

  for target in "${targets[@]}"; do
    build_for_target "$target"
  done

  echo "info: build complete"
  echo "info: linux artifacts:   $LINUX_DIST_DIR"
  echo "info: windows artifacts: $WINDOWS_DIST_DIR"
  echo
  ls -lh "$LINUX_DIST_DIR" || true
  ls -lh "$WINDOWS_DIST_DIR" || true
}

if [[ "${EC2_MANAGER_BUILD_LIB_ONLY:-0}" != "1" ]]; then
  main "$@"
fi
