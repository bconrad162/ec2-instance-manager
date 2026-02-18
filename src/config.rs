use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::error::Result;
use crate::models::{Mode, PortForwardPreset, RecentConnection, SavedFilter, TagMapping};
use crate::util::{home_dir, split_csv};

const APP_DIR: &str = "ec2-manager";
const FILE_NAME: &str = "config.ini";
const RECENTS_LIMIT: usize = 20;

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub default_mode: Mode,
    pub default_terminal: Option<String>,
    pub default_region: Option<String>,
    pub account_regions: BTreeMap<String, String>,
    pub tag_mapping: TagMapping,
    pub favorites: BTreeMap<String, Vec<String>>,
    pub recents: Vec<RecentConnection>,
    pub saved_filters: BTreeMap<String, Vec<SavedFilter>>,
    pub port_forward_presets: Vec<PortForwardPreset>,
    pub theme: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_mode: Mode::Live,
            default_terminal: None,
            default_region: Some("us-east-1".to_string()),
            account_regions: BTreeMap::new(),
            tag_mapping: TagMapping::default(),
            favorites: BTreeMap::new(),
            recents: Vec::new(),
            saved_filters: BTreeMap::new(),
            port_forward_presets: vec![
                PortForwardPreset {
                    name: "ssh-local".to_string(),
                    local_port: 2222,
                    remote_port: 22,
                },
                PortForwardPreset {
                    name: "postgres".to_string(),
                    local_port: 5432,
                    remote_port: 5432,
                },
            ],
            theme: None,
        }
    }
}

impl AppConfig {
    pub fn config_path() -> Option<PathBuf> {
        if cfg!(windows) {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .map(|p| p.join(APP_DIR).join(FILE_NAME))
                .or_else(|| {
                    home_dir().map(|p| {
                        p.join("AppData")
                            .join("Roaming")
                            .join(APP_DIR)
                            .join(FILE_NAME)
                    })
                })
        } else {
            home_dir().map(|p| p.join(".config").join(APP_DIR).join(FILE_NAME))
        }
    }

    pub fn load() -> Result<Self> {
        let Some(path) = Self::config_path() else {
            return Ok(Self::default());
        };

        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)?;
        Ok(Self::parse(&raw))
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(path, self.to_text())?;
        Ok(())
    }

    pub fn upsert_account_region(&mut self, account_id: &str, region: &str) {
        if !account_id.is_empty() && !region.is_empty() {
            self.account_regions
                .insert(account_id.to_string(), region.to_string());
        }
    }

    pub fn scope_key(account_id: &str, region: &str) -> String {
        format!("{}:{}", account_id.trim(), region.trim())
    }

    pub fn favorites_for_scope(&self, account_id: &str, region: &str) -> Vec<String> {
        let key = Self::scope_key(account_id, region);
        self.favorites.get(&key).cloned().unwrap_or_default()
    }

    pub fn is_favorite(&self, account_id: &str, region: &str, instance_id: &str) -> bool {
        let key = Self::scope_key(account_id, region);
        self.favorites
            .get(&key)
            .map(|v| v.iter().any(|id| id.eq_ignore_ascii_case(instance_id)))
            .unwrap_or(false)
    }

    pub fn toggle_favorite(&mut self, account_id: &str, region: &str, instance_id: &str) -> bool {
        let key = Self::scope_key(account_id, region);
        let entry = self.favorites.entry(key).or_default();
        if let Some(idx) = entry
            .iter()
            .position(|id| id.eq_ignore_ascii_case(instance_id))
        {
            entry.remove(idx);
            false
        } else {
            entry.push(instance_id.to_string());
            entry.sort();
            true
        }
    }

    pub fn add_recent_connection(&mut self, recent: RecentConnection) {
        self.recents.retain(|item| {
            !(item.account_id == recent.account_id
                && item.region == recent.region
                && item.instance_id.eq_ignore_ascii_case(&recent.instance_id))
        });
        self.recents.insert(0, recent);
        if self.recents.len() > RECENTS_LIMIT {
            self.recents.truncate(RECENTS_LIMIT);
        }
    }

    pub fn recents_for_scope(&self, account_id: &str, region: &str) -> Vec<RecentConnection> {
        self.recents
            .iter()
            .filter(|item| item.account_id == account_id && item.region == region)
            .cloned()
            .collect()
    }

    pub fn saved_filters_for_scope(&self, account_id: &str, region: &str) -> Vec<SavedFilter> {
        let key = Self::scope_key(account_id, region);
        self.saved_filters.get(&key).cloned().unwrap_or_default()
    }

    pub fn upsert_saved_filter(&mut self, account_id: &str, region: &str, saved: SavedFilter) {
        let key = Self::scope_key(account_id, region);
        let list = self.saved_filters.entry(key).or_default();

        if let Some(existing) = list
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(&saved.name))
        {
            *existing = saved;
            return;
        }

        list.push(saved);
        list.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
    }

    pub fn delete_saved_filter(&mut self, account_id: &str, region: &str, name: &str) -> bool {
        let key = Self::scope_key(account_id, region);
        let Some(list) = self.saved_filters.get_mut(&key) else {
            return false;
        };

        let before = list.len();
        list.retain(|item| !item.name.eq_ignore_ascii_case(name));
        before != list.len()
    }

    pub fn upsert_port_forward_preset(&mut self, preset: PortForwardPreset) {
        if let Some(existing) = self
            .port_forward_presets
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&preset.name))
        {
            *existing = preset;
            return;
        }

        self.port_forward_presets.push(preset);
        self.port_forward_presets.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        });
    }

    pub fn delete_port_forward_preset(&mut self, name: &str) -> bool {
        let before = self.port_forward_presets.len();
        self.port_forward_presets
            .retain(|preset| !preset.name.eq_ignore_ascii_case(name));
        before != self.port_forward_presets.len()
    }

    fn parse(raw: &str) -> Self {
        let mut cfg = Self::default();
        cfg.recents.clear();

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let Some((k, v)) = line.split_once('=') else {
                continue;
            };

            let key = k.trim();
            let value = v.trim();

            if let Some(rest) = key.strip_prefix("account_region.") {
                if !rest.is_empty() && !value.is_empty() {
                    cfg.account_regions
                        .insert(rest.to_string(), value.to_string());
                }
                continue;
            }

            if let Some(rest) = key.strip_prefix("favorite.") {
                if !rest.is_empty() {
                    cfg.favorites.insert(rest.to_string(), split_csv(value));
                }
                continue;
            }

            if let Some(rest) = key.strip_prefix("saved_filter.") {
                if rest.is_empty() || value.is_empty() {
                    continue;
                }
                if let Some(saved) = parse_saved_filter(value) {
                    cfg.saved_filters
                        .entry(rest.to_string())
                        .or_default()
                        .push(saved);
                }
                continue;
            }

            match key {
                "default_mode" => {
                    if let Some(parsed) = Mode::parse(value) {
                        cfg.default_mode = parsed;
                    }
                }
                "default_terminal" => {
                    cfg.default_terminal = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "default_region" => {
                    cfg.default_region = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "env_keys" => {
                    let parsed = split_csv(value);
                    if !parsed.is_empty() {
                        cfg.tag_mapping.env_keys = parsed;
                    }
                }
                "app_keys" => {
                    let parsed = split_csv(value);
                    if !parsed.is_empty() {
                        cfg.tag_mapping.app_keys = parsed;
                    }
                }
                "role_keys" => {
                    let parsed = split_csv(value);
                    if !parsed.is_empty() {
                        cfg.tag_mapping.role_keys = parsed;
                    }
                }
                "team_keys" => {
                    let parsed = split_csv(value);
                    if !parsed.is_empty() {
                        cfg.tag_mapping.team_keys = parsed;
                    }
                }
                "recent" => {
                    if let Some(recent) = parse_recent(value) {
                        cfg.recents.push(recent);
                    }
                }
                "port_forward_preset" => {
                    if let Some(preset) = parse_port_forward_preset(value) {
                        cfg.upsert_port_forward_preset(preset);
                    }
                }
                "theme" => {
                    cfg.theme = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                _ => {}
            }
        }

        cfg
    }

    fn to_text(&self) -> String {
        let mut lines = vec![
            format!("default_mode={}", self.default_mode.as_str()),
            format!(
                "default_terminal={}",
                self.default_terminal.clone().unwrap_or_default()
            ),
            format!(
                "default_region={}",
                self.default_region.clone().unwrap_or_default()
            ),
            format!("env_keys={}", self.tag_mapping.env_keys.join(",")),
            format!("app_keys={}", self.tag_mapping.app_keys.join(",")),
            format!("role_keys={}", self.tag_mapping.role_keys.join(",")),
            format!("team_keys={}", self.tag_mapping.team_keys.join(",")),
        ];

        if let Some(theme) = &self.theme {
            lines.push(format!("theme={theme}"));
        }

        for (account, region) in &self.account_regions {
            lines.push(format!("account_region.{account}={region}"));
        }

        for (scope, ids) in &self.favorites {
            lines.push(format!("favorite.{scope}={}", ids.join(",")));
        }

        for recent in &self.recents {
            lines.push(format!(
                "recent={}|{}|{}|{}|{}",
                recent.account_id,
                recent.region,
                recent.instance_id,
                recent.name.clone().unwrap_or_default(),
                recent.timestamp_unix
            ));
        }

        for (scope, filters) in &self.saved_filters {
            for saved in filters {
                lines.push(format!(
                    "saved_filter.{scope}={}|{}|{}|{}|{}",
                    saved.name,
                    saved.include_terms.join(","),
                    saved.exclude_terms.join(","),
                    saved.states.join(","),
                    if saved.only_ssm_managed { "1" } else { "0" }
                ));
            }
        }

        for preset in &self.port_forward_presets {
            lines.push(format!(
                "port_forward_preset={}|{}|{}",
                preset.name, preset.local_port, preset.remote_port
            ));
        }

        lines.push(String::new());
        lines.join("\n")
    }
}

fn parse_recent(raw: &str) -> Option<RecentConnection> {
    let fields: Vec<&str> = raw.split('|').collect();
    if fields.len() < 5 {
        return None;
    }

    Some(RecentConnection {
        account_id: fields[0].trim().to_string(),
        region: fields[1].trim().to_string(),
        instance_id: fields[2].trim().to_string(),
        name: {
            let value = fields[3].trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        },
        timestamp_unix: fields[4].trim().parse::<u64>().ok()?,
    })
}

fn parse_saved_filter(raw: &str) -> Option<SavedFilter> {
    let fields: Vec<&str> = raw.split('|').collect();
    if fields.len() < 4 {
        return None;
    }

    let name = fields[0].trim();
    if name.is_empty() {
        return None;
    }

    if fields.len() >= 5 {
        return Some(SavedFilter {
            name: name.to_string(),
            include_terms: split_csv(fields[1]),
            exclude_terms: split_csv(fields[2]),
            states: split_csv(fields[3]),
            only_ssm_managed: matches!(fields[4].trim(), "1" | "true" | "TRUE"),
        });
    }

    let include_terms = {
        let value = fields[1].trim();
        if value.is_empty() {
            Vec::new()
        } else {
            vec![value.to_string()]
        }
    };

    Some(SavedFilter {
        name: name.to_string(),
        include_terms,
        exclude_terms: Vec::new(),
        states: split_csv(fields[2]),
        only_ssm_managed: matches!(fields[3].trim(), "1" | "true" | "TRUE"),
    })
}

fn parse_port_forward_preset(raw: &str) -> Option<PortForwardPreset> {
    let fields: Vec<&str> = raw.split('|').collect();
    if fields.len() < 3 {
        return None;
    }

    let name = fields[0].trim();
    if name.is_empty() {
        return None;
    }

    Some(PortForwardPreset {
        name: name.to_string(),
        local_port: fields[1].trim().parse::<u16>().ok()?,
        remote_port: fields[2].trim().parse::<u16>().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let raw = "\
            default_mode=live\n\
            default_terminal=kitty\n\
            default_region=us-west-2\n\
            env_keys=Env,Environment\n\
            account_region.123=us-east-1\n\
            favorite.123:us-east-1=i-1,i-2\n\
            recent=123|us-east-1|i-1|api|1730000000\n\
            saved_filter.123:us-east-1=prod-only|prod|running|1\n\
            port_forward_preset=ssh|2222|22\n\
        ";

        let cfg = AppConfig::parse(raw);
        assert_eq!(cfg.default_mode, Mode::Live);
        assert_eq!(cfg.default_terminal.as_deref(), Some("kitty"));
        assert_eq!(cfg.default_region.as_deref(), Some("us-west-2"));
        assert_eq!(
            cfg.account_regions.get("123").map(String::as_str),
            Some("us-east-1")
        );
        assert_eq!(cfg.favorites_for_scope("123", "us-east-1").len(), 2);
        assert_eq!(cfg.recents.len(), 1);
        assert_eq!(cfg.saved_filters_for_scope("123", "us-east-1").len(), 1);
        assert!(cfg.port_forward_presets.iter().any(|p| p.name == "ssh"));
    }

    #[test]
    fn toggle_favorite_roundtrip() {
        let mut cfg = AppConfig::default();
        assert!(cfg.toggle_favorite("123", "us-east-1", "i-1"));
        assert!(cfg.is_favorite("123", "us-east-1", "i-1"));
        assert!(!cfg.toggle_favorite("123", "us-east-1", "i-1"));
        assert!(!cfg.is_favorite("123", "us-east-1", "i-1"));
    }

    #[test]
    fn recents_are_deduped_and_limited() {
        let mut cfg = AppConfig::default();
        cfg.recents.clear();

        for idx in 0..30u64 {
            cfg.add_recent_connection(RecentConnection {
                account_id: "123".to_string(),
                region: "us-east-1".to_string(),
                instance_id: format!("i-{idx}"),
                name: None,
                timestamp_unix: idx,
            });
        }

        assert_eq!(cfg.recents.len(), 20);
        assert_eq!(cfg.recents[0].instance_id, "i-29");

        cfg.add_recent_connection(RecentConnection {
            account_id: "123".to_string(),
            region: "us-east-1".to_string(),
            instance_id: "i-20".to_string(),
            name: Some("x".to_string()),
            timestamp_unix: 100,
        });

        assert_eq!(cfg.recents[0].instance_id, "i-20");
        assert_eq!(cfg.recents.len(), 20);
    }

    #[test]
    fn save_delete_filters() {
        let mut cfg = AppConfig::default();
        cfg.upsert_saved_filter(
            "123",
            "us-east-1",
            SavedFilter {
                name: "prod".to_string(),
                include_terms: vec!["prod".to_string()],
                exclude_terms: Vec::new(),
                states: vec!["running".to_string()],
                only_ssm_managed: true,
            },
        );

        assert_eq!(cfg.saved_filters_for_scope("123", "us-east-1").len(), 1);
        assert!(cfg.delete_saved_filter("123", "us-east-1", "prod"));
        assert_eq!(cfg.saved_filters_for_scope("123", "us-east-1").len(), 0);
    }

    #[test]
    fn config_roundtrip_keeps_new_fields() {
        let mut cfg = AppConfig::default();
        cfg.toggle_favorite("123", "us-east-1", "i-abc");
        cfg.add_recent_connection(RecentConnection {
            account_id: "123".to_string(),
            region: "us-east-1".to_string(),
            instance_id: "i-abc".to_string(),
            name: Some("api".to_string()),
            timestamp_unix: 11,
        });
        cfg.upsert_saved_filter(
            "123",
            "us-east-1",
            SavedFilter {
                name: "all".to_string(),
                include_terms: vec!["api".to_string(), "prod".to_string()],
                exclude_terms: vec!["legacy".to_string()],
                states: vec![],
                only_ssm_managed: false,
            },
        );
        cfg.upsert_port_forward_preset(PortForwardPreset {
            name: "mysql".to_string(),
            local_port: 3306,
            remote_port: 3306,
        });

        let raw = cfg.to_text();
        let parsed = AppConfig::parse(&raw);

        assert!(parsed.is_favorite("123", "us-east-1", "i-abc"));
        assert_eq!(parsed.recents_for_scope("123", "us-east-1").len(), 1);
        assert_eq!(parsed.saved_filters_for_scope("123", "us-east-1").len(), 1);
        let saved = parsed.saved_filters_for_scope("123", "us-east-1");
        assert_eq!(saved[0].include_terms, vec!["api", "prod"]);
        assert_eq!(saved[0].exclude_terms, vec!["legacy"]);
        assert!(parsed
            .port_forward_presets
            .iter()
            .any(|p| p.name == "mysql"));
    }

    #[test]
    fn parse_saved_filter_backward_compatible_with_old_format() {
        let raw = "saved_filter.123:us-east-1=prod-only|prod|running|1\n";
        let cfg = AppConfig::parse(raw);
        let saved = cfg.saved_filters_for_scope("123", "us-east-1");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].include_terms, vec!["prod"]);
        assert!(saved[0].exclude_terms.is_empty());
        assert_eq!(saved[0].states, vec!["running"]);
        assert!(saved[0].only_ssm_managed);
    }
}
