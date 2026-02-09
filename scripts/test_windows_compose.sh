#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${ROOT_DIR}/docker-compose.windows-test.yml"

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
assert_contains "windows-test:" "compose should define windows-test service"
assert_contains "image: dockurr/windows:latest" "compose should use dockurr/windows image"
assert_contains "privileged: true" "windows VM container should run privileged"
assert_contains "/dev/kvm" "compose should expose /dev/kvm device"
assert_contains "./dist/windows:/shared/dist/windows:ro" "compose should mount windows artifacts read-only"
assert_contains "3389:3389/tcp" "compose should expose RDP TCP port"
assert_contains "8006:8006" "compose should expose web viewer port"

echo "windows compose tests passed"
