#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.windows-gui-smoke.yml"

assert_contains() {
  local pattern="$1"
  local message="$2"
  if ! rg -q "$pattern" "$COMPOSE_FILE"; then
    echo "assertion failed: $message" >&2
    echo "missing pattern: $pattern" >&2
    exit 1
  fi
}

if [[ ! -f "$COMPOSE_FILE" ]]; then
  echo "assertion failed: compose file not found: $COMPOSE_FILE" >&2
  exit 1
fi

assert_contains "^services:" "compose should define services"
assert_contains "windows-gui-smoke:" "compose should define windows-gui-smoke service"
assert_contains "image: dockurr/windows:latest" "compose should use dockurr/windows image"
assert_contains "privileged: true" "windows VM should run privileged"
assert_contains "/dev/kvm" "compose should expose /dev/kvm device"
assert_contains "./windows-vm/smoke/share:/shared" "compose should mount smoke share folder"
assert_contains "./windows-vm/smoke/oem:/oem:ro" "compose should mount OEM hook folder read-only"
assert_contains "8016:8006" "compose should expose web viewer port"
assert_contains "3390:3389/tcp" "compose should expose RDP TCP port"

echo "windows gui smoke compose tests passed"
