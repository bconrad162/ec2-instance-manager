use crate::error::{AppError, Result};
use crate::models::Mode;

#[derive(Debug, Clone)]
pub struct GuiOptions {
    pub mode: Mode,
    pub region: Option<String>,
    pub dry_run: bool,
}

impl Default for GuiOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Live,
            region: None,
            dry_run: false,
        }
    }
}

pub fn parse_gui_args<I>(args: I) -> Result<GuiOptions>
where
    I: Iterator<Item = String>,
{
    let mut out = GuiOptions::default();
    let mut iter = args.peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mode" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--mode requires a value: live|sim".to_string())
                })?;

                out.mode = Mode::parse(&value).ok_or_else(|| {
                    AppError::InvalidArgument(format!("Unsupported --mode value: {value}"))
                })?;
            }
            "--region" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--region requires a value".to_string())
                })?;
                out.region = Some(value);
            }
            "--dry-run" => out.dry_run = true,
            "--no-dry-run" => out.dry_run = false,
            "-h" | "--help" => {}
            unknown => {
                return Err(AppError::InvalidArgument(format!(
                    "Unknown argument: {unknown}. Use --help"
                )))
            }
        }
    }

    Ok(out)
}

pub fn gui_help_text() -> &'static str {
    "ec2_manager_gui\n\
     \n\
     Usage:\n\
       cargo run --features gui --bin ec2_manager_gui -- [options]\n\
     \n\
     Options:\n\
       --mode live|sim      Startup mode (default live)\n\
       --region <region>    Startup region override\n\
       --dry-run            Keep launches in dry-run mode\n\
       --no-dry-run         Allow terminal/session launches (default)\n\
       -h, --help           Show help\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults() {
        let got = parse_gui_args(Vec::<String>::new().into_iter()).expect("default parse");
        assert_eq!(got.mode, Mode::Live);
        assert!(!got.dry_run);
    }

    #[test]
    fn parse_mode_and_region() {
        let got = parse_gui_args(
            vec![
                "--mode".to_string(),
                "live".to_string(),
                "--region".to_string(),
                "us-west-2".to_string(),
                "--no-dry-run".to_string(),
            ]
            .into_iter(),
        )
        .expect("parse should succeed");

        assert_eq!(got.mode, Mode::Live);
        assert_eq!(got.region.as_deref(), Some("us-west-2"));
        assert!(!got.dry_run);
    }
}
