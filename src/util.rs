use std::env;
use std::path::{Path, PathBuf};

pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn which_in_path(program: &str) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() && candidate.exists() {
        return Some(candidate.to_path_buf());
    }

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let full = dir.join(program);
        if full.is_file() {
            return Some(full);
        }
        if cfg!(windows) && !program.ends_with(".exe") {
            let exe_full = dir.join(format!("{program}.exe"));
            if exe_full.is_file() {
                return Some(exe_full);
            }
        }
    }

    None
}

pub fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn normalize_field(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("none")
        || trimmed.eq_ignore_ascii_case("null")
        || trimmed.eq_ignore_ascii_case("n/a")
    {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn truncate(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }

    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_len.saturating_sub(1) {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_csv_works() {
        let got = split_csv("a, b,, c");
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn normalize_field_works() {
        assert_eq!(normalize_field("None"), None);
        assert_eq!(normalize_field(" 10.0.0.1 "), Some("10.0.0.1".to_string()));
    }
}
