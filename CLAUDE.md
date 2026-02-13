# CLAUDE.md

## Project overview

Rust-only EC2 + SSM instance explorer with CLI (`ec2_manager`) and desktop GUI (`ec2_manager_gui`, egui/eframe). The GUI embeds a terminal for interactive SSM sessions.

## Build commands

```bash
# Development build (library + CLI)
cargo build

# Development build with GUI
cargo build --features gui

# Run tests
cargo test                  # lib + CLI tests
cargo test --features gui   # all tests including GUI (46 GUI tests)

# Clippy
cargo clippy --features gui

# Full release build (Linux + Windows cross-compile)
./scripts/build_binaries.sh         # or: ./scripts/build_binaries.sh all
./scripts/build_binaries.sh linux   # Linux only
./scripts/build_binaries.sh windows # Windows cross-compile only
```

## Build status

As of 2026-02-13, the full build pipeline passes cleanly:
- `cargo build --features gui` — zero warnings (Linux)
- `cargo test --features gui` — 83 tests pass (34 lib + 3 CLI + 46 GUI)
- `cargo clippy --features gui` — only pre-existing lib-level warnings (derivable_impls on Mode, too_many_arguments on sim::make_instance, collapsible_if in GUI)
- `./scripts/build_binaries.sh` — zero warnings on both Linux (x86_64-unknown-linux-gnu) and Windows (x86_64-pc-windows-gnu) release targets

## Architecture notes

### Windows embedded terminal

The GUI uses ConPTY (via `portable-pty`) exclusively for embedded terminal sessions. Only PowerShell 7, Windows PowerShell, and Command Prompt are supported as embedded shells on Windows. Git Bash/MSYS2/winpty are **not** used for embedded sessions.

Key functions:
- `filter_embedded_terminals()` in `ec2_manager_gui.rs` — restricts to PowerShell7/WindowsPowerShell/Cmd
- `spawn_pty_session_blocking()` — spawns all sessions via `native_pty_system()` (ConPTY on Windows)
- `pty_command_for_context()` — spawns `aws` directly with SSM args in live mode
- `resize_pty_session()` — propagates resize to both vt100 parser and PTY master

### cfg gates for imports

Test-only imports (`std::process::{Child, Command, Stdio}`) are gated with `#[cfg(test)]` to avoid unused-import warnings on both Linux native and Windows cross-compile targets. Similarly, `shell_plan()` is `#[cfg(test)]` since it's only used in tests.

## Key directories

- `src/` — Rust library and binaries
- `src/bin/ec2_manager_gui.rs` — GUI binary (requires `--features gui`)
- `scripts/` — Build and test helper scripts
- `dist/linux/` and `dist/windows/` — Release artifacts
- `windows-vm/smoke/` — Windows VM smoke test harness

## Smoke tests

```bash
# Shell script assertion tests (run locally without VM)
./scripts/test_run_windows_gui_smoke_test.sh
./scripts/test_windows_gui_smoke_compose.sh
./scripts/test_run_windows_vm_test.sh

# Full Windows VM smoke test (requires Docker + KVM)
./scripts/run_windows_gui_smoke_test.sh
```

The Windows smoke test uses PowerShell (`default_terminal=powershell`) in the guest VM.
