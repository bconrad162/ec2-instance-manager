use std::process::Command;

use crate::error::{AppError, Result};

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
