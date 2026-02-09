use std::fs;
use std::path::PathBuf;

use crate::error::{AppError, Result};
use crate::util::home_dir;

pub fn profile_choice_path() -> Option<PathBuf> {
    home_dir().map(|p| p.join(".aws").join("profileChoice"))
}

pub fn read_profile_choice() -> Result<String> {
    let Some(path) = profile_choice_path() else {
        return Err(AppError::NotFound(
            "Unable to resolve home directory for ~/.aws/profileChoice".to_string(),
        ));
    };

    let raw = fs::read_to_string(&path).map_err(|err| {
        AppError::Parse(format!("Failed to read {}: {err}", path.to_string_lossy()))
    })?;

    let profile = raw.trim();
    if profile.is_empty() {
        return Err(AppError::Parse(format!(
            "{} exists but is empty",
            path.to_string_lossy()
        )));
    }

    Ok(profile.to_string())
}
