use crate::config::AppConfig;
use crate::error::{AppError, Result};
use crate::models::{Instance, TerminalOption};
use crate::terminal::pick_default_terminal;

pub fn select_terminal(
    terminals: &[TerminalOption],
    config: &AppConfig,
    terminal_id: Option<&str>,
) -> Result<Option<TerminalOption>> {
    if let Some(id) = terminal_id {
        let found = terminals
            .iter()
            .find(|t| t.id.eq_ignore_ascii_case(id))
            .cloned();

        return found
            .map(Some)
            .ok_or_else(|| AppError::NotFound(format!("Terminal id not found: {id}")));
    }

    Ok(pick_default_terminal(config, terminals))
}

pub fn find_instance<'a>(instances: &'a [Instance], instance_id: &str) -> Option<&'a Instance> {
    instances
        .iter()
        .find(|i| i.instance_id.eq_ignore_ascii_case(instance_id))
}
