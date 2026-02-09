use std::path::PathBuf;
use std::process::Command;

use crate::config::AppConfig;
use crate::error::{AppError, Result};
use crate::models::{DependencyStatus, TerminalKind, TerminalOption};
use crate::util::which_in_path;

#[derive(Clone, Debug)]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub fn dependency_status() -> DependencyStatus {
    let aws_cli_found = which_in_path("aws").is_some();

    let ssm_plugin_found = if cfg!(windows) {
        which_in_path("session-manager-plugin.exe").is_some()
            || which_in_path("session-manager-plugin").is_some()
    } else {
        which_in_path("session-manager-plugin").is_some()
    };

    DependencyStatus {
        aws_cli_found,
        ssm_plugin_found,
    }
}

pub fn discover_terminals() -> Vec<TerminalOption> {
    let mut out = Vec::new();

    if cfg!(windows) {
        push_if_found(
            &mut out,
            "pwsh",
            "PowerShell 7",
            TerminalKind::PowerShell7,
            &["pwsh.exe"],
        );
        push_if_found(
            &mut out,
            "powershell",
            "Windows PowerShell",
            TerminalKind::WindowsPowerShell,
            &["powershell.exe"],
        );
        push_if_found(
            &mut out,
            "wt",
            "Windows Terminal",
            TerminalKind::WindowsTerminal,
            &["wt.exe"],
        );
        push_if_found(
            &mut out,
            "git-bash",
            "Git Bash",
            TerminalKind::GitBash,
            &["bash.exe"],
        );
        push_if_found(
            &mut out,
            "cmd",
            "Command Prompt",
            TerminalKind::Cmd,
            &["cmd.exe"],
        );
        push_if_found(&mut out, "wsl", "WSL", TerminalKind::Wsl, &["wsl.exe"]);

        for candidate in windows_git_bash_candidates() {
            if candidate.is_file() && !out.iter().any(|t| t.id == "git-bash") {
                out.push(TerminalOption {
                    id: "git-bash".to_string(),
                    display_name: "Git Bash".to_string(),
                    kind: TerminalKind::GitBash,
                    program: candidate.to_string_lossy().to_string(),
                });
            }
        }
    } else {
        push_if_found(
            &mut out,
            "cosmic-term",
            "COSMIC Terminal",
            TerminalKind::CosmicTerm,
            &["cosmic-term", "cosmic-terminal"],
        );
        push_if_found(
            &mut out,
            "gnome-terminal",
            "GNOME Terminal",
            TerminalKind::GnomeTerminal,
            &["gnome-terminal"],
        );
        push_if_found(&mut out, "kitty", "Kitty", TerminalKind::Kitty, &["kitty"]);
        push_if_found(
            &mut out,
            "alacritty",
            "Alacritty",
            TerminalKind::Alacritty,
            &["alacritty"],
        );
        push_if_found(
            &mut out,
            "konsole",
            "Konsole",
            TerminalKind::Konsole,
            &["konsole"],
        );
        push_if_found(&mut out, "xterm", "XTerm", TerminalKind::Xterm, &["xterm"]);
    }

    out
}

fn push_if_found(
    out: &mut Vec<TerminalOption>,
    id: &str,
    display_name: &str,
    kind: TerminalKind,
    candidates: &[&str],
) {
    for candidate in candidates {
        if let Some(path) = which_in_path(candidate) {
            out.push(TerminalOption {
                id: id.to_string(),
                display_name: display_name.to_string(),
                kind,
                program: path.to_string_lossy().to_string(),
            });
            return;
        }
    }
}

fn windows_git_bash_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        out.push(
            PathBuf::from(&program_files)
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        out.push(
            PathBuf::from(&program_files_x86)
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }

    out
}

pub fn pick_default_terminal(
    config: &AppConfig,
    terminals: &[TerminalOption],
) -> Option<TerminalOption> {
    if let Some(saved) = &config.default_terminal {
        if let Some(found) = terminals.iter().find(|t| &t.id == saved) {
            return Some(found.clone());
        }
    }

    let preferred = if cfg!(windows) {
        vec!["pwsh", "powershell", "wt", "cmd", "git-bash", "wsl"]
    } else {
        vec![
            "cosmic-term",
            "gnome-terminal",
            "kitty",
            "alacritty",
            "konsole",
            "xterm",
        ]
    };

    for id in preferred {
        if let Some(found) = terminals.iter().find(|t| t.id == id) {
            return Some(found.clone());
        }
    }

    terminals.first().cloned()
}

pub fn build_ssm_session_command(instance_id: &str, region: &str) -> String {
    format!("aws ssm start-session --target {instance_id} --region {region}")
}

pub fn build_ssm_port_forward_command(
    instance_id: &str,
    region: &str,
    local_port: u16,
    remote_port: u16,
) -> String {
    format!(
        "aws ssm start-session --target {instance_id} --region {region} --document-name AWS-StartPortForwardingSession --parameters localPortNumber={local_port},portNumber={remote_port}"
    )
}

pub fn build_launch_plan(
    terminal: &TerminalOption,
    session_command: &str,
    profile: &str,
    region: &str,
    tab_title: Option<&str>,
    prefer_tabs: bool,
) -> LaunchPlan {
    let mut args = Vec::new();
    let tab_title = tab_title.and_then(sanitize_tab_title);

    match terminal.kind {
        TerminalKind::PowerShell7 | TerminalKind::WindowsPowerShell => {
            args.push("-NoExit".to_string());
            args.push("-Command".to_string());
            args.push(session_command.to_string());
        }
        TerminalKind::WindowsTerminal => {
            args.push(if prefer_tabs {
                "new-tab".to_string()
            } else {
                "new-window".to_string()
            });
            if let Some(title) = &tab_title {
                args.push("--title".to_string());
                args.push(title.clone());
            }
            args.push("pwsh".to_string());
            args.push("-NoExit".to_string());
            args.push("-Command".to_string());
            args.push(session_command.to_string());
        }
        TerminalKind::Cmd => {
            args.push("/k".to_string());
            args.push(session_command.to_string());
        }
        TerminalKind::GitBash => {
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::Wsl => {
            args.push("--".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::CosmicTerm => {
            args.push("--".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::GnomeTerminal => {
            args.push(if prefer_tabs {
                "--tab".to_string()
            } else {
                "--window".to_string()
            });
            if let Some(title) = &tab_title {
                args.push(format!("--title={title}"));
            }
            args.push("--".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::Kitty => {
            if let Some(title) = &tab_title {
                args.push("--title".to_string());
                args.push(title.clone());
            }
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::Alacritty => {
            args.push("-e".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::Konsole => {
            if prefer_tabs {
                args.push("--new-tab".to_string());
            }
            if let Some(title) = &tab_title {
                args.push("-p".to_string());
                args.push(format!("tabtitle={title}"));
            }
            args.push("-e".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(format!("{}; exec bash", session_command));
        }
        TerminalKind::Xterm => {
            args.push("-hold".to_string());
            args.push("-e".to_string());
            args.push("bash".to_string());
            args.push("-lc".to_string());
            args.push(session_command.to_string());
        }
    }

    LaunchPlan {
        program: terminal.program.clone(),
        args,
        env: vec![
            ("AWS_PROFILE".to_string(), profile.to_string()),
            ("AWS_REGION".to_string(), region.to_string()),
        ],
    }
}

fn sanitize_tab_title(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_control() || ch == '"' || ch == '\'' {
            continue;
        }
        out.push(ch);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn launch(plan: &LaunchPlan, dry_run: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }

    let mut command = Command::new(&plan.program);
    command.args(&plan.args);

    for (k, v) in &plan.env {
        command.env(k, v);
    }

    command.spawn().map_err(|err| {
        AppError::Parse(format!(
            "Failed to launch terminal {}: {err}",
            &plan.program
        ))
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_linux_launch_plan() {
        let terminal = TerminalOption {
            id: "xterm".to_string(),
            display_name: "XTerm".to_string(),
            kind: TerminalKind::Xterm,
            program: "xterm".to_string(),
        };

        let plan = build_launch_plan(
            &terminal,
            "aws ssm start-session --target i-1 --region us-east-1",
            "dev",
            "us-east-1",
            Some("api-a"),
            true,
        );

        assert_eq!(plan.program, "xterm");
        assert!(plan.args.iter().any(|a| a == "-hold"));
        assert!(plan
            .env
            .iter()
            .any(|(k, v)| k == "AWS_PROFILE" && v == "dev"));
    }

    #[test]
    fn windows_terminal_uses_tab_title() {
        let terminal = TerminalOption {
            id: "wt".to_string(),
            display_name: "Windows Terminal".to_string(),
            kind: TerminalKind::WindowsTerminal,
            program: "wt.exe".to_string(),
        };

        let plan = build_launch_plan(
            &terminal,
            "echo hi",
            "dev",
            "us-east-1",
            Some("api-prod-1"),
            true,
        );

        assert_eq!(plan.args.first().map(String::as_str), Some("new-tab"));
        assert!(plan.args.iter().any(|a| a == "--title"));
        assert!(plan.args.iter().any(|a| a == "api-prod-1"));
    }

    #[test]
    fn gnome_terminal_uses_tab_mode_and_title() {
        let terminal = TerminalOption {
            id: "gnome-terminal".to_string(),
            display_name: "GNOME Terminal".to_string(),
            kind: TerminalKind::GnomeTerminal,
            program: "gnome-terminal".to_string(),
        };

        let plan = build_launch_plan(
            &terminal,
            "echo hi",
            "dev",
            "us-east-1",
            Some("api-prod-1"),
            true,
        );

        assert_eq!(plan.args.first().map(String::as_str), Some("--tab"));
        assert!(plan
            .args
            .iter()
            .any(|a| a == "--title=api-prod-1" || a.starts_with("--title=")));
    }

    #[test]
    fn build_port_forward_command() {
        let cmd = build_ssm_port_forward_command("i-abc", "us-east-1", 15432, 5432);
        assert!(cmd.contains("AWS-StartPortForwardingSession"));
        assert!(cmd.contains("localPortNumber=15432"));
        assert!(cmd.contains("portNumber=5432"));
    }

    #[test]
    fn build_port_forward_command_is_shell_neutral() {
        let cmd = build_ssm_port_forward_command("i-abc", "us-east-1", 15432, 5432);
        assert!(!cmd.contains('\''));
        assert!(!cmd.contains('"'));
    }
}
