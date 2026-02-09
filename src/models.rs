use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Live,
    Sim,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "live" => Some(Self::Live),
            "sim" | "simulation" => Some(Self::Sim),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Sim => "sim",
        }
    }
}

impl Default for Mode {
    fn default() -> Self {
        Self::Sim
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStatus {
    Ok,
    Expired,
    Missing,
}

impl fmt::Display for AuthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Expired => write!(f, "Expired"),
            Self::Missing => write!(f, "Missing"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AwsContext {
    pub mode: Mode,
    pub profile: String,
    pub account_id: Option<String>,
    pub arn: Option<String>,
    pub user_id: Option<String>,
    pub region: String,
    pub auth_status: AuthStatus,
}

#[derive(Clone, Debug)]
pub struct TagMapping {
    pub env_keys: Vec<String>,
    pub app_keys: Vec<String>,
    pub role_keys: Vec<String>,
    pub team_keys: Vec<String>,
}

impl Default for TagMapping {
    fn default() -> Self {
        Self {
            env_keys: vec!["Env".to_string(), "Environment".to_string()],
            app_keys: vec!["App".to_string(), "Service".to_string()],
            role_keys: vec!["Role".to_string(), "Tier".to_string()],
            team_keys: vec!["Team".to_string(), "Owner".to_string()],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub instance_id: String,
    pub state: String,
    pub private_ip: Option<String>,
    pub az: Option<String>,
    pub instance_type: Option<String>,
    pub launch_time: Option<String>,
    pub name: Option<String>,
    pub env: Option<String>,
    pub app_service: Option<String>,
    pub role: Option<String>,
    pub team_owner: Option<String>,
    pub asg: Option<String>,
    pub ssm_managed: bool,
    pub ssm_ping: Option<String>,
    pub ssm_last_ping: Option<String>,
    pub tags: BTreeMap<String, String>,
}

impl Instance {
    pub fn new(instance_id: String, state: String) -> Self {
        Self {
            instance_id,
            state,
            private_ip: None,
            az: None,
            instance_type: None,
            launch_time: None,
            name: None,
            env: None,
            app_service: None,
            role: None,
            team_owner: None,
            asg: None,
            ssm_managed: false,
            ssm_ping: None,
            ssm_last_ping: None,
            tags: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub instances: Vec<Instance>,
    pub fetched_at: SystemTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalKind {
    PowerShell7,
    WindowsPowerShell,
    WindowsTerminal,
    GitBash,
    Cmd,
    Wsl,
    CosmicTerm,
    GnomeTerminal,
    Kitty,
    Alacritty,
    Konsole,
    Xterm,
}

#[derive(Clone, Debug)]
pub struct TerminalOption {
    pub id: String,
    pub display_name: String,
    pub kind: TerminalKind,
    pub program: String,
}

#[derive(Clone, Debug)]
pub struct DependencyStatus {
    pub aws_cli_found: bool,
    pub ssm_plugin_found: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentConnection {
    pub account_id: String,
    pub region: String,
    pub instance_id: String,
    pub name: Option<String>,
    pub timestamp_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFilter {
    pub name: String,
    pub include_terms: Vec<String>,
    pub exclude_terms: Vec<String>,
    pub states: Vec<String>,
    pub only_ssm_managed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardPreset {
    pub name: String,
    pub local_port: u16,
    pub remote_port: u16,
}
