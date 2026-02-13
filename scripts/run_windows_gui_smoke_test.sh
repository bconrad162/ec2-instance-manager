#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.windows-gui-smoke.yml"
BUILD_SCRIPT="${ROOT_DIR}/scripts/build_binaries.sh"
SHARE_DIR="${ROOT_DIR}/windows-vm/smoke/share"
OEM_DIR="${ROOT_DIR}/windows-vm/smoke/oem"
RESULT_FILE="${SHARE_DIR}/windows_gui_smoke_result.txt"
MARKER_FILE="${SHARE_DIR}/windows_gui_smoke_marker.txt"
WEB_URL="http://localhost:8016"
VM_WAIT_SECONDS=600
SMOKE_WAIT_SECONDS=1200

usage() {
  cat <<USAGE
Usage: $0 [--skip-build] [--skip-host-gui-terminal-tests] [--reuse-storage] [--keep-running] [--vm-wait-seconds N] [--smoke-wait-seconds N]

Build Windows artifacts, run host-side GUI terminal validation, boot Windows VM smoke stack, and assert in-guest GUI terminal smoke marker using PowerShell.

Options:
  --skip-build                    Do not run Windows build step (expects dist/windows artifacts to exist)
  --skip-host-gui-terminal-tests  Skip host-side GUI terminal tests
  --reuse-storage                 Reuse existing VM storage volume (skip 'down -v' reset)
  --keep-running                  Leave VM stack running after script exits
  --vm-wait-seconds N             Timeout for VM web readiness (default: 600)
  --smoke-wait-seconds N          Timeout for in-guest smoke result file (default: 1200)
  -h, --help                      Show help
USAGE
}

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "error: required command not found: $cmd" >&2
    exit 1
  fi
}

assert_windows_artifacts_present() {
  local required=(
    "${ROOT_DIR}/dist/windows/ec2_manager.exe"
    "${ROOT_DIR}/dist/windows/ec2_manager_gui.exe"
    "${ROOT_DIR}/dist/windows/libgcc_s_seh-1.dll"
    "${ROOT_DIR}/dist/windows/libstdc++-6.dll"
    "${ROOT_DIR}/dist/windows/libwinpthread-1.dll"
  )

  local missing=0
  for file in "${required[@]}"; do
    if [[ ! -f "$file" ]]; then
      echo "error: missing required artifact: $file" >&2
      missing=1
    fi
  done
  if [[ "$missing" -ne 0 ]]; then
    exit 1
  fi
}

run_gui_terminal_validation_tests() {
  local -a tests=(
    "gui::tests::sim_mode_open_connection_tab_spawns_interactive_terminal_session"
    "gui::tests::terminal_event_payload_maps_ctrl_c_enter_and_paste"
    "gui::tests::terminal_text_with_cursor_places_cursor_marker"
    "gui::tests::gui_smoke_match_detects_expected_marker"
    "gui::tests::write_gui_smoke_marker_creates_parent_and_writes_payload"
  )

  for test_name in "${tests[@]}"; do
    echo "info: running host GUI terminal validation test: ${test_name}"
    cargo test --features gui --bin ec2_manager_gui "${test_name}"
  done
}

prepare_share_layout() {
  rm -rf "${SHARE_DIR}" "${OEM_DIR}"
  mkdir -p "${SHARE_DIR}/dist/windows" "${OEM_DIR}"

  cp "${ROOT_DIR}/dist/windows/ec2_manager.exe" "${SHARE_DIR}/dist/windows/"
  cp "${ROOT_DIR}/dist/windows/ec2_manager_gui.exe" "${SHARE_DIR}/dist/windows/"
  cp "${ROOT_DIR}/dist/windows/libgcc_s_seh-1.dll" "${SHARE_DIR}/dist/windows/"
  cp "${ROOT_DIR}/dist/windows/libstdc++-6.dll" "${SHARE_DIR}/dist/windows/"
  cp "${ROOT_DIR}/dist/windows/libwinpthread-1.dll" "${SHARE_DIR}/dist/windows/"

  rm -f "${RESULT_FILE}" "${MARKER_FILE}"

  cat > "${OEM_DIR}/install.bat" <<'BAT'
@echo off
setlocal
set LOG=C:\OEM\windows_gui_smoke_install.log
echo [info] starting windows gui smoke hook > "%LOG%"
powershell -NoProfile -ExecutionPolicy Bypass -File "C:\OEM\windows_gui_smoke.ps1" >> "%LOG%" 2>&1
if errorlevel 1 (
  echo [error] windows gui smoke hook failed >> "%LOG%"
) else (
  echo [info] windows gui smoke hook passed >> "%LOG%"
)
endlocal
BAT

  cat > "${OEM_DIR}/windows_gui_smoke.ps1" <<'PS1'
$ErrorActionPreference = "Stop"

$sharePath = "\\host.lan\Data"
$resultFile = Join-Path $sharePath "windows_gui_smoke_result.txt"
$markerFile = Join-Path $sharePath "windows_gui_smoke_marker.txt"
$exePath = Join-Path $sharePath "dist\windows\ec2_manager_gui.exe"
$workingDir = Split-Path -Parent $exePath
$appDataDir = Join-Path $sharePath "appdata"
$configDir = Join-Path $appDataDir "ec2-manager"
$configPath = Join-Path $configDir "config.ini"

for ($i = 0; $i -lt 120; $i++) {
  if (Test-Path $sharePath) { break }
  Start-Sleep -Seconds 2
}

if (-not (Test-Path $sharePath)) {
  exit 1
}

if (-not (Test-Path $exePath)) {
  Set-Content -Path $resultFile -Value "FAIL:missing-exe" -Encoding Ascii -Force
  exit 1
}

Remove-Item -Path $markerFile -ErrorAction SilentlyContinue
Remove-Item -Path $resultFile -ErrorAction SilentlyContinue

New-Item -ItemType Directory -Path $configDir -Force | Out-Null
Set-Content -Path $configPath -Value "default_mode=sim`ndefault_terminal=powershell`n" -Encoding Ascii -Force
$env:APPDATA = $appDataDir

$env:EC2_MANAGER_GUI_SMOKE_MARKER = $markerFile
$env:EC2_MANAGER_GUI_SMOKE_EXPECTED_TEXT = "[SIM MODE] session open for"
$env:EC2_MANAGER_GUI_SMOKE_EXIT_ON_MARKER = "1"
$env:EC2_MANAGER_GUI_SMOKE_AUTO_CONNECT = "1"

$proc = Start-Process -FilePath $exePath -ArgumentList "--mode", "sim", "--no-dry-run" -WorkingDirectory $workingDir -PassThru
$deadline = (Get-Date).AddSeconds(240)

while ((Get-Date) -lt $deadline) {
  if (Test-Path $markerFile) {
    Set-Content -Path $resultFile -Value "PASS`nterminal=powershell" -Encoding Ascii -Force
    if (-not $proc.HasExited) {
      Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
    exit 0
  }
  Start-Sleep -Seconds 2
  $proc.Refresh()
  if ($proc.HasExited) {
    Set-Content -Path $resultFile -Value "FAIL:gui-exited:$($proc.ExitCode)" -Encoding Ascii -Force
    exit 1
  }
}

Set-Content -Path $resultFile -Value "FAIL:timeout" -Encoding Ascii -Force
if (-not $proc.HasExited) {
  Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
}
exit 1
PS1
}

wait_for_web_ready() {
  local end_time
  end_time=$((SECONDS + VM_WAIT_SECONDS))
  while (( SECONDS < end_time )); do
    if curl -fsS "${WEB_URL}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 3
  done
  return 1
}

wait_for_smoke_result() {
  local end_time
  end_time=$((SECONDS + SMOKE_WAIT_SECONDS))
  while (( SECONDS < end_time )); do
    if [[ -f "${RESULT_FILE}" ]]; then
      return 0
    fi
    sleep 5
  done
  return 1
}

main() {
  local skip_build=0
  local skip_host_tests=0
  local keep_running=0
  local reuse_storage=0

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --skip-build)
        skip_build=1
        shift
        ;;
      --skip-host-gui-terminal-tests)
        skip_host_tests=1
        shift
        ;;
      --reuse-storage)
        reuse_storage=1
        shift
        ;;
      --keep-running)
        keep_running=1
        shift
        ;;
      --vm-wait-seconds)
        VM_WAIT_SECONDS="${2:-}"
        if [[ -z "$VM_WAIT_SECONDS" || ! "$VM_WAIT_SECONDS" =~ ^[0-9]+$ ]]; then
          echo "error: --vm-wait-seconds requires a positive integer" >&2
          exit 1
        fi
        shift 2
        ;;
      --smoke-wait-seconds)
        SMOKE_WAIT_SECONDS="${2:-}"
        if [[ -z "$SMOKE_WAIT_SECONDS" || ! "$SMOKE_WAIT_SECONDS" =~ ^[0-9]+$ ]]; then
          echo "error: --smoke-wait-seconds requires a positive integer" >&2
          exit 1
        fi
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        echo "error: unknown argument: $1" >&2
        usage
        exit 1
        ;;
    esac
  done

  require_cmd docker
  require_cmd curl
  if [[ "$skip_host_tests" -eq 0 ]]; then
    require_cmd cargo
  fi

  if [[ ! -f "$COMPOSE_FILE" ]]; then
    echo "error: compose file not found: $COMPOSE_FILE" >&2
    exit 1
  fi

  if [[ "$skip_build" -eq 0 ]]; then
    echo "info: building windows artifacts"
    "$BUILD_SCRIPT" windows
  fi

  assert_windows_artifacts_present

  if [[ "$skip_host_tests" -eq 0 ]]; then
    run_gui_terminal_validation_tests
  fi

  prepare_share_layout

  if [[ "$reuse_storage" -eq 0 ]]; then
    echo "info: resetting VM storage volume for deterministic /oem hook execution"
    docker compose -f "$COMPOSE_FILE" down -v --remove-orphans >/dev/null 2>&1 || true
  fi

  if [[ "$keep_running" -eq 0 ]]; then
    trap 'docker compose -f "$COMPOSE_FILE" down >/dev/null 2>&1 || true' EXIT
  fi

  echo "info: starting windows GUI smoke stack"
  docker compose -f "$COMPOSE_FILE" up -d

  echo "info: waiting for VM web readiness at ${WEB_URL} (timeout ${VM_WAIT_SECONDS}s)"
  if ! wait_for_web_ready; then
    echo "error: VM web endpoint did not become ready in time" >&2
    echo "hint: inspect logs with:" >&2
    echo "  docker compose -f $COMPOSE_FILE logs --tail=200" >&2
    exit 1
  fi

  echo "info: waiting for in-guest GUI smoke result file (timeout ${SMOKE_WAIT_SECONDS}s)"
  if ! wait_for_smoke_result; then
    echo "error: timed out waiting for smoke result file: ${RESULT_FILE}" >&2
    echo "hint: first-time Windows installs can take a long time; rerun with larger --smoke-wait-seconds" >&2
    echo "hint: inspect logs with: docker compose -f $COMPOSE_FILE logs --tail=200" >&2
    exit 1
  fi

  local result
  result="$(head -n 1 "${RESULT_FILE}" | tr -d '\r')"
  if [[ "$result" == "PASS" ]]; then
    echo "info: Windows GUI smoke test passed"
    echo "info: result file: ${RESULT_FILE}"
    echo "info: marker file: ${MARKER_FILE}"
    return 0
  fi

  echo "error: Windows GUI smoke test failed with result: ${result}" >&2
  echo "info: result file: ${RESULT_FILE}" >&2
  exit 1
}

main "$@"
