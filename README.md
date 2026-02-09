# ec2_manager

Rust-only EC2 + SSM instance explorer with:
- CLI (`ec2_manager`)
- Desktop GUI (`ec2_manager_gui`, Rust `egui/eframe`)

## What is implemented

- Reads AWS profile from `~/.aws/profileChoice` (live mode), with sim fallback when missing.
- Resolves auth context and region (override/env/aws config/config fallback).
- Fetches EC2 inventory + SSM managed/ping status (live mode via AWS CLI).
- Fast client-side filtering (`--search`, `--state`, `--only-ssm`).
  - `--search` / `--include` include term(s), `--exclude` removes matches.
  - Search matches across instance name, private IP, instance ID, and all tags.
- Terminal discovery and launch adapters (Linux + Windows definitions).
- SSM connect and port-forward launch plans.
- Favorites, recents, saved filters, and port-forward presets persisted to config.
- Diagnostics for auth/dependencies/permissions.
- Interactive Rust shell mode (`--interactive`) for local operation without JS/HTML.

## Build

```bash
cargo build
```

Build GUI binary too:

```bash
cargo build --features gui --bin ec2_manager_gui
```

Build scripts:

```bash
# host-native build
./scripts/build_binaries.sh

# Pop!_OS linux + Windows artifacts
./scripts/build_binaries.sh all
```

Artifacts are written to:
- `dist/linux/`
- `dist/windows/`

Expected files:
- `dist/linux/ec2_manager`
- `dist/linux/ec2_manager_gui`
- `dist/windows/ec2_manager.exe`
- `dist/windows/ec2_manager_gui.exe`
- `dist/windows/libgcc_s_seh-1.dll`
- `dist/windows/libstdc++-6.dll`
- `dist/windows/libwinpthread-1.dll`

## Launch Desktop GUI (Pop!_OS 24.04)

From source:

```bash
cargo run --features gui --bin ec2_manager_gui
```

From built binary:

```bash
./dist/ec2_manager_gui
```

Flags:
- `--mode sim` for local simulation
- `--mode live` for AWS live mode (default)
- `--dry-run` prevents launching terminal sessions
- `--no-dry-run` allows real terminal/session launches (default)

## Sim mode (Pop!_OS)

```bash
cargo run -- --mode sim
```

Filter + dry-run connect:

```bash
cargo run -- --mode sim --search prod --only-ssm --connect i-sim0001 --dry-run
```

Interactive mode:

```bash
cargo run -- --mode sim --interactive --dry-run
```

## Live mode (Windows)

Requirements in `PATH`:

- `aws`
- `session-manager-plugin`
- one supported terminal (`pwsh`, `powershell`, `wt`, `cmd`, `bash`, or `wsl`)

### Windows setup (binary distribution)

1. Copy the entire `dist/windows/` folder to your Windows laptop.
2. Keep the `.exe` files and the three DLLs in the same folder:
   - `ec2_manager.exe`
   - `ec2_manager_gui.exe`
   - `libgcc_s_seh-1.dll`
   - `libstdc++-6.dll`
   - `libwinpthread-1.dll`
3. Install AWS tools on Windows:
   - AWS CLI v2
   - Session Manager Plugin
4. Ensure AWS auth/profile setup exists:
   - `%USERPROFILE%\\.aws\\profileChoice`
   - plus normal AWS config/credentials as needed by your profile.
5. Launch GUI:
   - `ec2_manager_gui.exe` (defaults to `--mode live`)

If Windows reports a missing DLL, it means the DLL is not next to the `.exe`.

### Exactly where DLL files go

Place the DLLs in the **same directory** as the executable you run.

Example target layout on Windows:

```text
C:\tools\ec2-manager\windows\
  ec2_manager.exe
  ec2_manager_gui.exe
  libgcc_s_seh-1.dll
  libstdc++-6.dll
  libwinpthread-1.dll
```

Run from that folder:

```powershell
cd C:\tools\ec2-manager\windows
.\ec2_manager_gui.exe
```

Do not put these DLLs in `C:\Windows\System32`; keep them beside the `.exe`.

### Windows sim-mode quick check

```powershell
.\ec2_manager_gui.exe --mode sim --dry-run
```

Commands:

```bash
cargo run -- --mode live --refresh
cargo run -- --mode live --refresh --connect i-0123456789abcdef0 --terminal pwsh
cargo run -- --mode live --port-forward i-0123456789abcdef0 --local-port 15432 --remote-port 5432 --terminal pwsh
```

## Windows VM test on Linux via Docker Compose

When you want to smoke-test Windows artifacts from a Linux workstation, use:

```bash
docker compose -f docker-compose.windows-test.yml up -d
```

Notes:
- This uses `dockurr/windows` (Windows VM in a container), not native Windows containers.
- Requires hardware virtualization support (`/dev/kvm`) and Docker privileges.
- Windows build artifacts are mounted read-only from `./dist/windows` into the container path `/shared/dist/windows`.
- Exposed ports:
  - `8006` web viewer
  - `3389` RDP

Validate compose config quickly:

```bash
./scripts/test_windows_compose.sh
```

One-command helper to build, run terminal-specific GUI validation tests, start the VM, and verify:

```bash
./scripts/run_windows_vm_test.sh
```

Skip terminal-specific GUI validation tests if needed:

```bash
./scripts/run_windows_vm_test.sh --skip-gui-terminal-tests
```

Helper script tests:

```bash
./scripts/test_run_windows_vm_test.sh
```

Automated Windows in-guest GUI terminal smoke test (MSYS2 terminal only; uses `/oem/install.bat` hook and shared result marker):

```bash
./scripts/run_windows_gui_smoke_test.sh
```

If MSYS2 is installed in a non-default location, override detection path:

```bash
./scripts/run_windows_gui_smoke_test.sh --msys2-bash-path 'C:\custom\msys64\usr\bin\bash.exe'
```

Related smoke harness checks:

```bash
./scripts/test_windows_gui_smoke_compose.sh
./scripts/test_run_windows_gui_smoke_test.sh
```

## Useful options

- `--interactive`
- `--include <csv>`
- `--exclude <csv>`
- `--list-terminals`
- `--watch-profile`
- `--diagnostics`
- `--favorite <instance>`
- `--list-favorites`
- `--list-recents`
- `--save-filter <name>`
- `--apply-filter <name>`
- `--region us-east-1`
- `--dry-run`

## Run And Test Script

```bash
./scripts/run_and_test.sh
```

## Config path

- Linux: `~/.config/ec2-manager/config.ini`
- Windows: `%APPDATA%\ec2-manager\config.ini`
