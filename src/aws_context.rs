use std::fs;

use crate::aws_cli::run_aws_cli;
use crate::config::AppConfig;
use crate::error::Result;
use crate::models::{AuthStatus, AwsContext, Mode};
use crate::profile_choice::read_profile_choice;
use crate::util::home_dir;

pub fn build_context(
    mode: Mode,
    config: &AppConfig,
    region_override: Option<&str>,
) -> Result<AwsContext> {
    let profile_res = read_profile_choice();

    let profile = match profile_res {
        Ok(value) => value,
        Err(_) if mode == Mode::Sim => "sim-profile".to_string(),
        Err(_) => String::new(),
    };

    if mode == Mode::Sim {
        let region = resolve_region(&profile, config, None, region_override);
        return Ok(AwsContext {
            mode,
            profile,
            account_id: Some("000000000000".to_string()),
            arn: Some("arn:aws:iam::000000000000:user/sim-user".to_string()),
            user_id: Some("SIMUSER".to_string()),
            region,
            auth_status: AuthStatus::Ok,
        });
    }

    if profile.is_empty() {
        let region = resolve_region("", config, None, region_override);
        return Ok(AwsContext {
            mode,
            profile,
            account_id: None,
            arn: None,
            user_id: None,
            region,
            auth_status: AuthStatus::Missing,
        });
    }

    let identity = sts_get_caller_identity(&profile);
    match identity {
        Ok((account_id, arn, user_id)) => {
            let region = resolve_region(&profile, config, Some(&account_id), region_override);
            Ok(AwsContext {
                mode,
                profile,
                account_id: Some(account_id),
                arn: Some(arn),
                user_id: Some(user_id),
                region,
                auth_status: AuthStatus::Ok,
            })
        }
        Err(_) => {
            let region = resolve_region(&profile, config, None, region_override);
            Ok(AwsContext {
                mode,
                profile,
                account_id: None,
                arn: None,
                user_id: None,
                region,
                auth_status: AuthStatus::Expired,
            })
        }
    }
}

fn sts_get_caller_identity(profile: &str) -> Result<(String, String, String)> {
    let output = run_aws_cli(
        Some(profile),
        None,
        &[
            "sts",
            "get-caller-identity",
            "--output",
            "text",
            "--query",
            "[Account,Arn,UserId]",
        ],
    )?;

    parse_sts_identity(&output)
}

fn parse_sts_identity(raw: &str) -> Result<(String, String, String)> {
    let mut parts = raw.split_whitespace();
    let account = parts.next().unwrap_or_default().to_string();
    let arn = parts.next().unwrap_or_default().to_string();
    let user_id = parts.next().unwrap_or_default().to_string();

    if account.is_empty() || arn.is_empty() || user_id.is_empty() {
        return Err(crate::error::AppError::Parse(
            "STS output was missing fields".to_string(),
        ));
    }

    Ok((account, arn, user_id))
}

fn resolve_region(
    profile: &str,
    config: &AppConfig,
    account_id: Option<&str>,
    region_override: Option<&str>,
) -> String {
    if let Some(region) = region_override {
        if !region.trim().is_empty() {
            return region.trim().to_string();
        }
    }

    if let Some(account) = account_id {
        if let Some(region) = config.account_regions.get(account) {
            if !region.trim().is_empty() {
                return region.clone();
            }
        }
    }

    if let Ok(region) = std::env::var("AWS_REGION") {
        if !region.trim().is_empty() {
            return region;
        }
    }

    if let Ok(region) = std::env::var("AWS_DEFAULT_REGION") {
        if !region.trim().is_empty() {
            return region;
        }
    }

    if let Some(region) = read_profile_region_from_aws_config(profile) {
        return region;
    }

    config
        .default_region
        .clone()
        .unwrap_or_else(|| "us-east-1".to_string())
}

fn read_profile_region_from_aws_config(profile: &str) -> Option<String> {
    if profile.trim().is_empty() {
        return None;
    }

    let path = home_dir()?.join(".aws").join("config");
    let content = fs::read_to_string(path).ok()?;

    let header = if profile == "default" {
        "[default]".to_string()
    } else {
        format!("[profile {profile}]")
    };

    let mut in_section = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line.eq_ignore_ascii_case(&header);
            continue;
        }

        if !in_section || line.starts_with('#') || line.is_empty() {
            continue;
        }

        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("region") {
                let value = v.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sts_fields() {
        let raw = "123456789012\tarn:aws:iam::123456789012:role/Admin\tABC";
        let got = parse_sts_identity(raw).expect("expected STS output to parse");
        assert_eq!(got.0, "123456789012");
        assert!(got.1.contains("arn:aws:iam"));
        assert_eq!(got.2, "ABC");
    }
}
