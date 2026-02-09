#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.windows-test.yml"
BUILD_SCRIPT="${ROOT_DIR}/scripts/build_binaries.sh"
WEB_URL="http://localhost:8006"
WAIT_SECONDS=180

usage() {
  cat <<USAGE
Usage: $0 [--skip-build] [--skip-gui-terminal-tests] [--wait-seconds N]

Build Windows artifacts, run GUI terminal validation tests, start the Windows VM test environment, and validate readiness.

Options:
  --skip-build                Do not run build step (expects dist/windows artifacts to exist)
  --skip-gui-terminal-tests   Skip terminal-specific GUI validation tests
  --wait-seconds N            Timeout for web readiness check (default: 180)
  -h, --help                  Show help
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

wait_for_web_ready() {
  local end_time
  end_time=$((SECONDS + WAIT_SECONDS))

  while (( SECONDS < end_time )); do
    if curl -fsS "$WEB_URL" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
  done

  return 1
}

run_gui_terminal_validation_tests() {
  local -a tests=(
    "gui::tests::sim_mode_open_connection_tab_spawns_interactive_terminal_session"
    "gui::tests::terminal_event_payload_maps_ctrl_c_enter_and_paste"
    "gui::tests::terminal_text_with_cursor_places_cursor_marker"
  )

  for test_name in "${tests[@]}"; do
    echo "info: running GUI terminal validation test: ${test_name}"
    cargo test --features gui --bin ec2_manager_gui "${test_name}"
  done
}

main() {
  local skip_build=0
  local skip_gui_terminal_tests=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --skip-build)
        skip_build=1
        shift
        ;;
      --skip-gui-terminal-tests)
        skip_gui_terminal_tests=1
        shift
        ;;
      --wait-seconds)
        WAIT_SECONDS="${2:-}"
        if [[ -z "$WAIT_SECONDS" || ! "$WAIT_SECONDS" =~ ^[0-9]+$ ]]; then
          echo "error: --wait-seconds requires a positive integer" >&2
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
  if [[ "$skip_gui_terminal_tests" -eq 0 ]]; then
    require_cmd cargo
  fi

  if [[ ! -f "$COMPOSE_FILE" ]]; then
    echo "error: compose file not found: $COMPOSE_FILE" >&2
    exit 1
  fi

  if [[ "$skip_build" -eq 0 ]]; then
    echo "info: building linux + windows artifacts"
    "$BUILD_SCRIPT" all
  fi

  if [[ "$skip_gui_terminal_tests" -eq 0 ]]; then
    run_gui_terminal_validation_tests
  fi

  assert_windows_artifacts_present

  echo "info: starting windows vm compose stack"
  docker compose -f "$COMPOSE_FILE" up -d

  echo "info: waiting for web viewer readiness at $WEB_URL (timeout ${WAIT_SECONDS}s)"
  if ! wait_for_web_ready; then
    echo "error: windows vm web viewer did not become ready in time" >&2
    echo "hint: inspect logs with:" >&2
    echo "  docker compose -f $COMPOSE_FILE logs --tail=200" >&2
    exit 1
  fi

  echo "info: windows vm test environment is ready"
  echo "info: web viewer: $WEB_URL"
  echo "info: rdp: localhost:3389"
  echo "info: shared artifacts path in VM container: /shared/dist/windows"
}

main "$@"
