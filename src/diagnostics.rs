use crate::aws_cli::run_aws_cli;
use crate::config::AppConfig;
use crate::models::{AuthStatus, AwsContext, DependencyStatus, Mode, TerminalOption};
use crate::profile_choice;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Failed(String),
    Skipped(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsReport {
    pub mode: String,
    pub profile: String,
    pub profile_choice_exists: bool,
    pub auth_status: String,
    pub aws_cli_found: bool,
    pub ssm_plugin_found: bool,
    pub terminal_count: usize,
    pub default_terminal: Option<String>,
    pub ec2_check: CheckStatus,
    pub ssm_check: CheckStatus,
}

pub fn run_diagnostics(
    context: &AwsContext,
    deps: &DependencyStatus,
    terminals: &[TerminalOption],
    config: &AppConfig,
) -> DiagnosticsReport {
    let profile_choice_exists = profile_choice::profile_choice_path()
        .map(|p| p.exists())
        .unwrap_or(false);

    let mut report = DiagnosticsReport {
        mode: context.mode.as_str().to_string(),
        profile: context.profile.clone(),
        profile_choice_exists,
        auth_status: context.auth_status.to_string(),
        aws_cli_found: deps.aws_cli_found,
        ssm_plugin_found: deps.ssm_plugin_found,
        terminal_count: terminals.len(),
        default_terminal: config.default_terminal.clone(),
        ec2_check: CheckStatus::Skipped("not evaluated".to_string()),
        ssm_check: CheckStatus::Skipped("not evaluated".to_string()),
    };

    if context.mode == Mode::Sim {
        report.ec2_check = CheckStatus::Skipped("sim mode".to_string());
        report.ssm_check = CheckStatus::Skipped("sim mode".to_string());
        return report;
    }

    if context.auth_status != AuthStatus::Ok {
        report.ec2_check = CheckStatus::Skipped("auth not OK".to_string());
        report.ssm_check = CheckStatus::Skipped("auth not OK".to_string());
        return report;
    }

    if !deps.aws_cli_found {
        report.ec2_check = CheckStatus::Failed("aws CLI not found in PATH".to_string());
        report.ssm_check = CheckStatus::Failed("aws CLI not found in PATH".to_string());
        return report;
    }

    report.ec2_check = permission_check_ec2(&context.profile, &context.region);
    report.ssm_check = permission_check_ssm(&context.profile, &context.region);
    report
}

fn permission_check_ec2(profile: &str, region: &str) -> CheckStatus {
    match run_aws_cli(
        Some(profile),
        Some(region),
        &[
            "ec2",
            "describe-instances",
            "--max-items",
            "1",
            "--output",
            "text",
            "--query",
            "Reservations[].Instances[].InstanceId",
        ],
    ) {
        Ok(_) => CheckStatus::Ok,
        Err(err) => CheckStatus::Failed(err.to_string()),
    }
}

fn permission_check_ssm(profile: &str, region: &str) -> CheckStatus {
    match run_aws_cli(
        Some(profile),
        Some(region),
        &[
            "ssm",
            "describe-instance-information",
            "--max-results",
            "1",
            "--output",
            "text",
            "--query",
            "InstanceInformationList[].InstanceId",
        ],
    ) {
        Ok(_) => CheckStatus::Ok,
        Err(err) => CheckStatus::Failed(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AuthStatus, AwsContext, Mode, TerminalKind, TerminalOption};

    #[test]
    fn sim_diagnostics_skip_live_checks() {
        let context = AwsContext {
            mode: Mode::Sim,
            profile: "sim-profile".to_string(),
            account_id: Some("000000000000".to_string()),
            arn: None,
            user_id: None,
            region: "us-east-1".to_string(),
            auth_status: AuthStatus::Ok,
        };

        let report = run_diagnostics(
            &context,
            &DependencyStatus {
                aws_cli_found: false,
                ssm_plugin_found: false,
            },
            &[TerminalOption {
                id: "xterm".to_string(),
                display_name: "XTerm".to_string(),
                kind: TerminalKind::Xterm,
                program: "xterm".to_string(),
            }],
            &AppConfig::default(),
        );

        assert_eq!(report.mode, "sim");
        assert_eq!(report.terminal_count, 1);
        assert_eq!(
            report.ec2_check,
            CheckStatus::Skipped("sim mode".to_string())
        );
        assert_eq!(
            report.ssm_check,
            CheckStatus::Skipped("sim mode".to_string())
        );
    }

    #[test]
    fn auth_not_ok_skips_permission_checks() {
        let context = AwsContext {
            mode: Mode::Live,
            profile: "dev".to_string(),
            account_id: None,
            arn: None,
            user_id: None,
            region: "us-east-1".to_string(),
            auth_status: AuthStatus::Expired,
        };

        let report = run_diagnostics(
            &context,
            &DependencyStatus {
                aws_cli_found: true,
                ssm_plugin_found: true,
            },
            &[],
            &AppConfig::default(),
        );

        assert_eq!(
            report.ec2_check,
            CheckStatus::Skipped("auth not OK".to_string())
        );
        assert_eq!(
            report.ssm_check,
            CheckStatus::Skipped("auth not OK".to_string())
        );
    }
}
