use std::process::Command;

use crate::error::{AppError, Result};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn run_aws_cli(profile: Option<&str>, region: Option<&str>, args: &[&str]) -> Result<String> {
    let mut final_args: Vec<String> = Vec::new();

    if let Some(p) = profile {
        final_args.push("--profile".to_string());
        final_args.push(p.to_string());
    }

    if let Some(r) = region {
        final_args.push("--region".to_string());
        final_args.push(r.to_string());
    }

    final_args.extend(args.iter().map(|s| s.to_string()));

    run_capture("aws", &final_args, &[])
}

pub fn run_capture(program: &str, args: &[String], envs: &[(String, String)]) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    for (k, v) in envs {
        cmd.env(k, v);
    }

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(AppError::CommandFailed {
            program: program.to_string(),
            args: args.to_vec(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
