use std::fmt;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug)]
pub enum AppError {
    Io(std::io::Error),
    Parse(String),
    InvalidArgument(String),
    CommandFailed {
        program: String,
        args: Vec<String>,
        stderr: String,
    },
    NotFound(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Parse(msg) => write!(f, "Parse error: {msg}"),
            Self::InvalidArgument(msg) => write!(f, "Invalid argument: {msg}"),
            Self::CommandFailed {
                program,
                args,
                stderr,
            } => {
                write!(
                    f,
                    "Command failed: {} {}\n{}",
                    program,
                    args.join(" "),
                    stderr.trim()
                )
            }
            Self::NotFound(msg) => write!(f, "Not found: {msg}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
