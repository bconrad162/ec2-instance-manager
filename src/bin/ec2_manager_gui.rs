#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!(
        "GUI is disabled in this build. Run:\n  cargo run --features gui --bin ec2_manager_gui -- --mode sim"
    );
}

#[cfg(feature = "gui")]
mod gui {
    use std::collections::{HashMap, VecDeque};
    use std::fs;
    use std::io::{Read, Write};
    #[cfg(target_os = "windows")]
    use std::io::BufReader;
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;
    use std::process::{Child, ChildStdin};
    #[cfg(any(target_os = "windows", test))]
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex, Once};
    use std::time::{Duration, Instant, SystemTime};

    use eframe::egui;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    use ec2_manager::aws_context::build_context;
    use ec2_manager::config::AppConfig;
    use ec2_manager::connection_tabs::ConnectionTabs;
    use ec2_manager::diagnostics::run_diagnostics;
    use ec2_manager::error::{AppError, Result};
    use ec2_manager::filter::{apply_filters, Filters};
    use ec2_manager::gui_cli::{gui_help_text, parse_gui_args, GuiOptions};
    use ec2_manager::inventory::load_inventory;
    use ec2_manager::models::{
        AuthStatus, AwsContext, DependencyStatus, Instance, Inventory, Mode, SavedFilter,
        TerminalOption,
    };
    use ec2_manager::profile_choice::profile_choice_path;
    use ec2_manager::terminal::{
        build_ssm_port_forward_command, build_ssm_session_command, dependency_status,
        discover_terminals, pick_default_terminal,
    };
    use ec2_manager::util::truncate;
    use ec2_manager::workflow::find_instance;

    const GUI_DEFAULT_WIDTH: f32 = 1720.0;
    const GUI_DEFAULT_HEIGHT: f32 = 980.0;
    const GUI_MIN_WIDTH: f32 = 1280.0;
    const GUI_MIN_HEIGHT: f32 = 760.0;
    const PROFILE_POLL_INTERVAL: Duration = Duration::from_secs(1);
    const PROFILE_CHANGE_DEBOUNCE: Duration = Duration::from_secs(2);
    const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(450);
    const GUI_SMOKE_MARKER_ENV: &str = "EC2_MANAGER_GUI_SMOKE_MARKER";
    const GUI_SMOKE_EXPECTED_TEXT_ENV: &str = "EC2_MANAGER_GUI_SMOKE_EXPECTED_TEXT";
    const GUI_SMOKE_EXIT_ON_MARKER_ENV: &str = "EC2_MANAGER_GUI_SMOKE_EXIT_ON_MARKER";
    const GUI_SMOKE_AUTO_CONNECT_ENV: &str = "EC2_MANAGER_GUI_SMOKE_AUTO_CONNECT";
    #[cfg(target_os = "windows")]
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    const COL_FAV_W: f32 = 44.0;
    const COL_INSTANCE_W: f32 = 150.0;
    const COL_NAME_W: f32 = 220.0;
    const COL_STATE_W: f32 = 90.0;
    const COL_SSM_W: f32 = 70.0;
    const COL_IP_W: f32 = 130.0;
    const COL_ENV_W: f32 = 70.0;
    const COL_APP_W: f32 = 260.0;
    const STATE_FILTER_NONE: &str = "";
    const STATE_FILTER_RUNNING: &str = "running";
    const STATE_FILTER_STOPPED: &str = "stopped";
    const STATE_FILTER_TERMINATED: &str = "terminated";
    const AWS_REGION_AUTO: &str = "(auto)";
    const AWS_REGIONS: &[&str] = &[
        "us-east-1",
        "us-east-2",
        "us-west-1",
        "us-west-2",
        "ca-central-1",
        "sa-east-1",
        "eu-west-1",
        "eu-west-2",
        "eu-west-3",
        "eu-central-1",
        "eu-central-2",
        "eu-north-1",
        "eu-south-1",
        "eu-south-2",
        "me-south-1",
        "me-central-1",
        "af-south-1",
        "ap-south-1",
        "ap-south-2",
        "ap-east-1",
        "ap-southeast-1",
        "ap-southeast-2",
        "ap-southeast-3",
        "ap-southeast-4",
        "ap-northeast-1",
        "ap-northeast-2",
        "ap-northeast-3",
    ];

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MainTab {
        Inventory,
        Connections,
        Log,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum LogLevel {
        Error,
        Warn,
        Info,
        Debug,
        Trace,
    }

    impl LogLevel {
        fn as_str(self) -> &'static str {
            match self {
                Self::Error => "ERROR",
                Self::Warn => "WARN",
                Self::Info => "INFO",
                Self::Debug => "DEBUG",
                Self::Trace => "TRACE",
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct LogEntry {
        level: LogLevel,
        message: String,
    }

    #[derive(Clone, Debug)]
    struct LogFilters {
        error: bool,
        warn: bool,
        info: bool,
        debug: bool,
        trace: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct GuiSmokeConfig {
        marker_path: PathBuf,
        expected_text: String,
        exit_on_marker: bool,
        auto_connect: bool,
    }

    impl Default for LogFilters {
        fn default() -> Self {
            Self {
                error: true,
                warn: true,
                info: true,
                debug: false,
                trace: false,
            }
        }
    }

    impl LogFilters {
        fn includes(&self, level: LogLevel) -> bool {
            match level {
                LogLevel::Error => self.error,
                LogLevel::Warn => self.warn,
                LogLevel::Info => self.info,
                LogLevel::Debug => self.debug,
                LogLevel::Trace => self.trace,
            }
        }

        fn set_verbosity_low(&mut self) {
            self.error = true;
            self.warn = true;
            self.info = true;
            self.debug = false;
            self.trace = false;
        }

        fn set_verbosity_medium(&mut self) {
            self.error = true;
            self.warn = true;
            self.info = true;
            self.debug = true;
            self.trace = false;
        }

        fn set_verbosity_high(&mut self) {
            self.error = true;
            self.warn = true;
            self.info = true;
            self.debug = true;
            self.trace = true;
        }
    }

    enum ProcEvent {
        Output { tab_id: u64, bytes: Vec<u8> },
        Exited { tab_id: u64, code: i32 },
        Error { tab_id: u64, error: String },
    }

    struct PtySession {
        child: Box<dyn portable_pty::Child + Send>,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        parser: vt100::Parser,
    }

    struct PipeSession {
        child: Child,
        stdin: Arc<Mutex<ChildStdin>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RowAction {
        select: bool,
        connect: bool,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SearchRuleKind {
        Include,
        Exclude,
    }

    #[derive(Clone, Debug)]
    struct SearchRuleInput {
        kind: SearchRuleKind,
        term: String,
    }

    impl Default for SearchRuleInput {
        fn default() -> Self {
            Self {
                kind: SearchRuleKind::Include,
                term: String::new(),
            }
        }
    }

    fn search_terms_from_rules(rules: &[SearchRuleInput]) -> (Vec<String>, Vec<String>) {
        let mut includes = Vec::new();
        let mut excludes = Vec::new();

        for rule in rules {
            let term = rule.term.trim();
            if term.is_empty() {
                continue;
            }
            match rule.kind {
                SearchRuleKind::Include => includes.push(term.to_string()),
                SearchRuleKind::Exclude => excludes.push(term.to_string()),
            }
        }

        (includes, excludes)
    }

    fn rules_from_search_terms(includes: &[String], excludes: &[String]) -> Vec<SearchRuleInput> {
        let mut rules = Vec::new();

        for term in includes {
            rules.push(SearchRuleInput {
                kind: SearchRuleKind::Include,
                term: term.clone(),
            });
        }
        for term in excludes {
            rules.push(SearchRuleInput {
                kind: SearchRuleKind::Exclude,
                term: term.clone(),
            });
        }

        if rules.is_empty() {
            rules.push(SearchRuleInput::default());
        }

        rules
    }

    fn states_from_state_filter(selected_state_filter: &str) -> Vec<String> {
        let state = selected_state_filter.trim();
        if state.is_empty() {
            Vec::new()
        } else {
            vec![state.to_string()]
        }
    }

    fn state_filter_from_saved_states(saved_states: &[String]) -> String {
        let Some(first) = saved_states.first() else {
            return STATE_FILTER_NONE.to_string();
        };

        let normalized = first.trim().to_ascii_lowercase();
        match normalized.as_str() {
            STATE_FILTER_RUNNING | STATE_FILTER_STOPPED | STATE_FILTER_TERMINATED => normalized,
            _ => STATE_FILTER_NONE.to_string(),
        }
    }

    fn selected_region_label(selected_region: Option<&str>, context_region: Option<&str>) -> String {
        if let Some(region) = selected_region {
            return region.to_string();
        }
        match context_region {
            Some(region) => format!("{AWS_REGION_AUTO} ({region})"),
            None => AWS_REGION_AUTO.to_string(),
        }
    }

    fn panic_log_path() -> PathBuf {
        AppConfig::config_path()
            .map(|p| p.with_file_name("ec2_manager_gui_panic.log"))
            .unwrap_or_else(|| std::env::temp_dir().join("ec2_manager_gui_panic.log"))
    }

    fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
        if let Some(message) = payload.downcast_ref::<&'static str>() {
            return (*message).to_string();
        }
        if let Some(message) = payload.downcast_ref::<String>() {
            return message.clone();
        }
        "non-string panic payload".to_string()
    }

    fn append_panic_log_entry(entry: &str) {
        let path = panic_log_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{entry}");
        }
    }

    fn install_gui_panic_hook() {
        static PANIC_HOOK_ONCE: Once = Once::new();
        PANIC_HOOK_ONCE.call_once(|| {
            let default_hook = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                let location = info
                    .location()
                    .map(|loc| format!("{}:{}", loc.file(), loc.line()))
                    .unwrap_or_else(|| "unknown-location".to_string());
                let payload = panic_payload_to_string(info.payload());
                let message = format!("panic captured: {payload} @ {location}");
                append_panic_log_entry(&message);
                eprintln!("error: {message}");
                default_hook(info);
            }));
        });
    }

    pub fn run() {
        install_gui_panic_hook();
        if std::env::args().any(|a| a == "--help" || a == "-h") {
            println!("{}", gui_help_text());
            return;
        }

        let options = match parse_gui_args(std::env::args().skip(1)) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("error: {err}");
                eprintln!("\n{}", gui_help_text());
                std::process::exit(1);
            }
        };

        let native_options = default_native_options();
        let title = "EC2 + SSM Instance Explorer";
        let app_options = options.clone();

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            eframe::run_native(
                title,
                native_options,
                Box::new(move |_cc| Ok(Box::new(Ec2GuiApp::new(app_options.clone())))),
            )
        }));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                let message = format!("failed to start GUI: {err}");
                append_panic_log_entry(&message);
                eprintln!("error: {message}");
                std::process::exit(1);
            }
            Err(payload) => {
                let message = format!(
                    "GUI bootstrap panic: {}",
                    panic_payload_to_string(payload.as_ref())
                );
                append_panic_log_entry(&message);
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    }

    fn default_native_options() -> eframe::NativeOptions {
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([GUI_DEFAULT_WIDTH, GUI_DEFAULT_HEIGHT])
                .with_min_inner_size([GUI_MIN_WIDTH, GUI_MIN_HEIGHT]),
            ..Default::default()
        }
    }

    struct Ec2GuiApp {
        options: GuiOptions,
        gui_smoke: Option<GuiSmokeConfig>,
        gui_smoke_marker_written: bool,
        gui_smoke_auto_connect_attempted: bool,
        gui_smoke_should_close: bool,
        config: AppConfig,
        context: Option<AwsContext>,
        dependencies: DependencyStatus,
        inventory: Inventory,
        filtered: Vec<Instance>,

        search_rules: Vec<SearchRuleInput>,
        selected_state_filter: String,
        only_ssm: bool,
        save_filter_name: String,
        selected_saved_filter: String,
        selected_instance_id: String,
        local_port: u16,
        remote_port: u16,

        message: String,
        diagnostics: String,

        main_tab: MainTab,
        logs: VecDeque<LogEntry>,
        log_filters: LogFilters,
        terminals: Vec<TerminalOption>,
        selected_terminal_id: String,
        profile_choice_path: Option<PathBuf>,
        last_profile_choice_mtime: Option<SystemTime>,
        pending_profile_choice_mtime: Option<SystemTime>,
        pending_profile_change_since: Option<SystemTime>,
        last_profile_poll_at: Instant,
        connections: ConnectionTabs,
        pty_sessions: HashMap<u64, PtySession>,
        pipe_sessions: HashMap<u64, PipeSession>,
        proc_tx: Sender<ProcEvent>,
        proc_rx: Receiver<ProcEvent>,
    }

    impl Ec2GuiApp {
        const MAX_LOG_LINES: usize = 20_000;

        fn new(options: GuiOptions) -> Self {
            let config = AppConfig::load().unwrap_or_default();
            let dependencies = dependency_status();
            let (proc_tx, proc_rx) = mpsc::channel();
            let profile_choice_path = profile_choice_path();
            let last_profile_choice_mtime = profile_choice_mtime(profile_choice_path.as_deref());
            let terminals = discover_terminals();
            let selected_terminal_id = initial_terminal_id(&config, &terminals);
            let gui_smoke = gui_smoke_config_from_env();

            let mut app = Self {
                options,
                gui_smoke,
                gui_smoke_marker_written: false,
                gui_smoke_auto_connect_attempted: false,
                gui_smoke_should_close: false,
                config,
                context: None,
                dependencies,
                inventory: Inventory {
                    instances: Vec::new(),
                    fetched_at: std::time::SystemTime::now(),
                },
                filtered: Vec::new(),
                search_rules: vec![SearchRuleInput::default()],
                selected_state_filter: STATE_FILTER_NONE.to_string(),
                only_ssm: false,
                save_filter_name: String::new(),
                selected_saved_filter: String::new(),
                selected_instance_id: String::new(),
                local_port: 2222,
                remote_port: 22,
                message: String::new(),
                diagnostics: String::new(),
                main_tab: MainTab::Inventory,
                logs: VecDeque::new(),
                log_filters: LogFilters::default(),
                terminals,
                selected_terminal_id,
                profile_choice_path,
                last_profile_choice_mtime,
                pending_profile_choice_mtime: None,
                pending_profile_change_since: None,
                last_profile_poll_at: Instant::now(),
                connections: ConnectionTabs::new(),
                pty_sessions: HashMap::new(),
                pipe_sessions: HashMap::new(),
                proc_tx,
                proc_rx,
            };

            app.log_info("application started");
            if let Some(smoke) = &app.gui_smoke {
                app.log_info(format!(
                    "GUI smoke mode enabled marker={} expected='{}'",
                    smoke.marker_path.display(),
                    smoke.expected_text
                ));
            }
            if let Err(err) = app.refresh_context_and_inventory(true) {
                app.message = format!("error: {err}");
                app.log_error(app.message.clone());
            }
            app.maybe_auto_connect_gui_smoke();
            app
        }

        fn poll_profile_choice_changes(&mut self) {
            if self.last_profile_poll_at.elapsed() < PROFILE_POLL_INTERVAL {
                return;
            }
            self.last_profile_poll_at = Instant::now();
            let now = SystemTime::now();

            let current_mtime = profile_choice_mtime(self.profile_choice_path.as_deref());

            // Reset pending state if file is back to the already-applied mtime.
            if current_mtime == self.last_profile_choice_mtime {
                self.pending_profile_choice_mtime = None;
                self.pending_profile_change_since = None;
                return;
            }

            // Start or restart debounce window when mtime changes.
            if self.pending_profile_choice_mtime != current_mtime {
                self.pending_profile_choice_mtime = current_mtime;
                self.pending_profile_change_since = Some(now);
                self.log_debug("profileChoice changed; waiting for debounce window");
                return;
            }

            if !profile_change_debounce_elapsed(
                self.pending_profile_change_since,
                now,
                PROFILE_CHANGE_DEBOUNCE,
            ) {
                return;
            }

            self.pending_profile_choice_mtime = None;
            self.pending_profile_change_since = None;
            self.last_profile_choice_mtime = current_mtime;
            self.log_info("detected profileChoice change, refreshing context and inventory");
            if let Err(err) = self.refresh_context_and_inventory(true) {
                self.message = format!("error: {err}");
                self.log_error(self.message.clone());
            }
        }

        fn log(&mut self, level: LogLevel, message: impl Into<String>) {
            let mut message = message.into();
            if message.trim().is_empty() {
                message = "<empty>".to_string();
            }
            self.logs.push_back(LogEntry { level, message });
            if self.logs.len() > Self::MAX_LOG_LINES {
                let overflow = self.logs.len() - Self::MAX_LOG_LINES;
                self.logs.drain(0..overflow);
            }
        }

        fn log_error(&mut self, message: impl Into<String>) {
            self.log(LogLevel::Error, message);
        }

        fn log_warn(&mut self, message: impl Into<String>) {
            self.log(LogLevel::Warn, message);
        }

        fn log_info(&mut self, message: impl Into<String>) {
            self.log(LogLevel::Info, message);
        }

        fn log_debug(&mut self, message: impl Into<String>) {
            self.log(LogLevel::Debug, message);
        }

        fn selected_terminal(&self) -> Option<&TerminalOption> {
            self.terminals
                .iter()
                .find(|t| t.id == self.selected_terminal_id)
        }

        fn refresh_context_and_inventory(&mut self, force: bool) -> Result<()> {
            self.log_info(format!("refresh inventory requested (force={force})"));
            let context = build_context(
                self.options.mode.clone(),
                &self.config,
                self.options.region.as_deref(),
            )?;

            if let Some(account_id) = &context.account_id {
                if let Some(region) = &self.options.region {
                    self.config.upsert_account_region(account_id, region);
                    self.config.save()?;
                }
            }

            self.context = Some(context.clone());

            if context.mode == Mode::Live && context.auth_status != AuthStatus::Ok {
                self.inventory = Inventory {
                    instances: Vec::new(),
                    fetched_at: std::time::SystemTime::now(),
                };
                self.filtered.clear();
                self.message =
                    "Auth is not OK (live mode). Refresh credentials and retry.".to_string();
                self.log_warn("inventory refresh blocked: auth not OK in live mode");
                return Ok(());
            }

            self.inventory = load_inventory(&context, &self.config.tag_mapping, force)?;
            self.apply_filters();
            self.message = format!(
                "Loaded {} instances ({} filtered)",
                self.inventory.instances.len(),
                self.filtered.len()
            );
            self.log_info(self.message.clone());
            self.maybe_auto_connect_gui_smoke();
            Ok(())
        }

        fn apply_filters(&mut self) {
            let (includes, excludes) = search_terms_from_rules(&self.search_rules);
            self.filtered = apply_filters(
                &self.inventory.instances,
                &Filters {
                    includes,
                    excludes,
                    states: states_from_state_filter(&self.selected_state_filter),
                    only_ssm_managed: self.only_ssm,
                },
            );
            self.log_debug(format!(
                "filters applied -> {} visible / {} total",
                self.filtered.len(),
                self.inventory.instances.len()
            ));
        }

        fn maybe_auto_connect_gui_smoke(&mut self) {
            if self.gui_smoke_auto_connect_attempted {
                return;
            }
            let Some(smoke) = &self.gui_smoke else {
                return;
            };
            if !smoke.auto_connect {
                self.gui_smoke_auto_connect_attempted = true;
                return;
            }
            self.gui_smoke_auto_connect_attempted = true;

            if self.options.mode != Mode::Sim {
                self.log_warn("GUI smoke auto-connect only runs in sim mode");
                return;
            }

            let Some(instance_id) = self
                .filtered
                .iter()
                .find(|i| i.ssm_managed)
                .or_else(|| self.filtered.first())
                .map(|i| i.instance_id.clone())
            else {
                self.log_error("GUI smoke auto-connect failed: no instances available");
                return;
            };

            self.selected_instance_id = instance_id;
            match self.connect_selected() {
                Ok(()) => self.log_info("GUI smoke auto-connect succeeded"),
                Err(err) => self.log_error(format!("GUI smoke auto-connect failed: {err}")),
            }
        }

        fn maybe_record_gui_smoke_success(&mut self, tab_id: u64, bytes: &[u8]) {
            let Some(smoke) = &self.gui_smoke else {
                return;
            };
            let marker_path = smoke.marker_path.clone();
            let expected_text = smoke.expected_text.clone();
            let exit_on_marker = smoke.exit_on_marker;
            if self.gui_smoke_marker_written {
                return;
            }
            if !gui_smoke_match_in_bytes(&expected_text, bytes) {
                return;
            }

            match write_gui_smoke_marker(&marker_path, tab_id, &expected_text) {
                Ok(()) => {
                    self.gui_smoke_marker_written = true;
                    self.log_info(format!(
                        "GUI smoke marker written to {}",
                        marker_path.display()
                    ));
                    if exit_on_marker {
                        self.gui_smoke_should_close = true;
                    }
                }
                Err(err) => {
                    self.log_error(format!(
                        "failed to write GUI smoke marker {}: {err}",
                        marker_path.display()
                    ));
                }
            }
        }

        fn account_scope(&self) -> String {
            self.context
                .as_ref()
                .and_then(|c| c.account_id.clone())
                .unwrap_or_else(|| "unknown-account".to_string())
        }

        fn region_scope(&self) -> String {
            self.context
                .as_ref()
                .map(|c| c.region.clone())
                .unwrap_or_else(|| {
                    self.options
                        .region
                        .clone()
                        .unwrap_or_else(|| "us-east-1".to_string())
                })
        }

        fn selected_instance(&self) -> Option<&Instance> {
            if self.selected_instance_id.trim().is_empty() {
                return None;
            }
            find_instance(&self.filtered, self.selected_instance_id.trim()).or_else(|| {
                find_instance(&self.inventory.instances, self.selected_instance_id.trim())
            })
        }

        fn connect_selected(&mut self) -> Result<()> {
            let context = self
                .context
                .clone()
                .ok_or_else(|| AppError::Parse("Context not loaded".to_string()))?;
            let instance = self
                .selected_instance()
                .ok_or_else(|| AppError::NotFound("Select an instance first".to_string()))?
                .clone();
            self.log_info(format!(
                "connect requested for {}",
                instance.instance_id
            ));

            if context.mode == Mode::Live
                && (!self.dependencies.aws_cli_found || !self.dependencies.ssm_plugin_found)
            {
                return Err(AppError::Parse(
                    "Connect requires aws CLI + session-manager-plugin in PATH".to_string(),
                ));
            }

            let cmd = build_ssm_session_command(&instance.instance_id, &context.region);
            let command = if context.mode == Mode::Sim {
                format!(
                    "echo '[SIM MODE] {cmd}'; echo '[SIM MODE] session open for {}'",
                    instance.instance_id
                )
            } else {
                cmd
            };

            let title = instance
                .name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| instance.instance_id.clone());

            self.open_connection_tab(title, instance.instance_id.clone(), command, &context)?;
            self.main_tab = MainTab::Connections;
            Ok(())
        }

        fn port_forward_selected(&mut self) -> Result<()> {
            let context = self
                .context
                .clone()
                .ok_or_else(|| AppError::Parse("Context not loaded".to_string()))?;
            let instance = self
                .selected_instance()
                .ok_or_else(|| AppError::NotFound("Select an instance first".to_string()))?
                .clone();
            self.log_info(format!(
                "port-forward requested for {} local={} remote={}",
                instance.instance_id, self.local_port, self.remote_port
            ));

            if context.mode == Mode::Live
                && (!self.dependencies.aws_cli_found || !self.dependencies.ssm_plugin_found)
            {
                return Err(AppError::Parse(
                    "Port forward requires aws CLI + session-manager-plugin in PATH".to_string(),
                ));
            }

            let cmd = build_ssm_port_forward_command(
                &instance.instance_id,
                &context.region,
                self.local_port,
                self.remote_port,
            );

            let command = if context.mode == Mode::Sim {
                format!(
                    "echo '[SIM MODE] {cmd}'; echo '[SIM MODE] port-forward {}:{} for {}'",
                    self.local_port, self.remote_port, instance.instance_id
                )
            } else {
                cmd
            };

            let title = format!(
                "{} pf {}:{}",
                instance
                    .name
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| instance.instance_id.clone()),
                self.local_port,
                self.remote_port
            );

            self.open_connection_tab(title, instance.instance_id.clone(), command, &context)?;
            self.main_tab = MainTab::Connections;
            Ok(())
        }

        fn open_connection_tab(
            &mut self,
            title: String,
            instance_id: String,
            command: String,
            context: &AwsContext,
        ) -> Result<()> {
            let selected_terminal = self.selected_terminal().cloned();
            let tab_id = self.connections.open(title.clone(), instance_id.clone());
            self.log_info(format!(
                "opened connection tab id={tab_id} instance={instance_id}"
            ));
            self.connections
                .append_line(tab_id, format!("$ {}", truncate(&command, 220)));
            self.connections.append_line(
                tab_id,
                format!(
                    "profile={} region={} mode={}",
                    context.profile,
                    context.region,
                    context.mode.as_str()
                ),
            );
            if let Some(terminal) = &selected_terminal {
                self.connections.append_line(
                    tab_id,
                    format!("[shell profile] {} ({})", terminal.display_name, terminal.id),
                );
            }
            self.connections.append_line(
                tab_id,
                "SSM command is auto-sent. Click terminal area to focus and type directly."
                    .to_string(),
            );

            if context.mode == Mode::Live {
                self.connections.append_line(
                    tab_id,
                    "note: live SSM start-session may require a full TTY; embedded mode is best-effort.".to_string(),
                );
            }

            if self.options.dry_run {
                self.connections
                    .append_line(tab_id, "[dry-run] launch skipped".to_string());
                self.connections.set_running(tab_id, false);
                self.message = "Opened connection tab (dry-run)".to_string();
                self.log_info("dry-run mode: process launch skipped");
                return Ok(());
            }

            #[cfg(target_os = "windows")]
            {
                if should_try_pty() {
                    if let Err(err) =
                        self.spawn_pty_session(tab_id, selected_terminal.as_ref(), context)
                    {
                        self.log_warn(format!(
                            "PTY session failed on Windows; falling back to pipe mode: {err}"
                        ));
                        self.spawn_pipe_session(tab_id, selected_terminal.as_ref(), context)?;
                    }
                } else {
                    self.spawn_pipe_session(tab_id, selected_terminal.as_ref(), context)?;
                }
            }
            #[cfg(not(target_os = "windows"))]
            self.spawn_pty_session(tab_id, selected_terminal.as_ref(), context)?;
            if should_auto_send_prefilled_command(self.options.dry_run) {
                let mut payload = command.into_bytes();
                payload.push(b'\n');
                self.send_raw_bytes_to_connection_tab(tab_id, &payload);
                self.log_info(format!("tab={tab_id} auto-sent prefilled command"));
            }

            if !self.options.dry_run {
                self.config
                    .add_recent_connection(ec2_manager::models::RecentConnection {
                        account_id: self.account_scope(),
                        region: self.region_scope(),
                        instance_id,
                        name: Some(title),
                        timestamp_unix: now_unix(),
                    });
                self.config.save()?;
            }

            self.message = "Opened connection tab".to_string();
            Ok(())
        }

        #[cfg(target_os = "windows")]
        fn spawn_pipe_session(
            &mut self,
            tab_id: u64,
            terminal: Option<&TerminalOption>,
            context: &AwsContext,
        ) -> Result<()> {
            let (program, args) = shell_plan(terminal);
            self.log_debug(format!("spawning pipe shell via {program}"));

            let mut command = Command::new(program);
            command
                .args(args)
                .env("AWS_PROFILE", &context.profile)
                .env("AWS_REGION", &context.region)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW);
            let mut child = command
                .spawn()
                .map_err(|err| AppError::Parse(format!("Failed to start shell: {err}")))?;

            if let Some(stdout) = child.stdout.take() {
                let tx = self.proc_tx.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut buf = vec![0_u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _ = tx.send(ProcEvent::Output {
                                    tab_id,
                                    bytes: buf[..n].to_vec(),
                                });
                            }
                            Err(err) => {
                                let _ = tx.send(ProcEvent::Error {
                                    tab_id,
                                    error: err.to_string(),
                                });
                                break;
                            }
                        }
                    }
                });
            }

            if let Some(stderr) = child.stderr.take() {
                let tx = self.proc_tx.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut buf = vec![0_u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _ = tx.send(ProcEvent::Output {
                                    tab_id,
                                    bytes: buf[..n].to_vec(),
                                });
                            }
                            Err(err) => {
                                let _ = tx.send(ProcEvent::Error {
                                    tab_id,
                                    error: err.to_string(),
                                });
                                break;
                            }
                        }
                    }
                });
            }

            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| AppError::Parse("Failed to acquire child stdin".to_string()))?;

            self.pipe_sessions.insert(
                tab_id,
                PipeSession {
                    child,
                    stdin: Arc::new(Mutex::new(stdin)),
                },
            );
            Ok(())
        }

        fn spawn_pty_session(
            &mut self,
            tab_id: u64,
            terminal: Option<&TerminalOption>,
            context: &AwsContext,
        ) -> Result<()> {
            let (program, args) = shell_plan(terminal);
            self.log_debug(format!("spawning PTY shell via {program}"));

            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: 45,
                    cols: 180,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|err| AppError::Parse(format!("Failed to allocate PTY: {err}")))?;

            let mut cmd = CommandBuilder::new(program);
            for arg in args {
                cmd.arg(arg);
            }
            cmd.env("AWS_PROFILE", &context.profile);
            cmd.env("AWS_REGION", &context.region);

            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|err| AppError::Parse(format!("Failed to spawn PTY shell: {err}")))?;
            drop(pair.slave);

            let mut reader = pair
                .master
                .try_clone_reader()
                .map_err(|err| AppError::Parse(format!("Failed to create PTY reader: {err}")))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| AppError::Parse(format!("Failed to create PTY writer: {err}")))?;

            let tx = self.proc_tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0_u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = tx.send(ProcEvent::Output {
                                tab_id,
                                bytes: buf[..n].to_vec(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(ProcEvent::Error {
                                tab_id,
                                error: err.to_string(),
                            });
                            break;
                        }
                    }
                }
            });

            self.pty_sessions.insert(
                tab_id,
                PtySession {
                    child,
                    writer: Arc::new(Mutex::new(writer)),
                    parser: vt100::Parser::new(45, 180, 10_000),
                },
            );
            Ok(())
        }

        fn poll_connection_events(&mut self) {
            while let Ok(event) = self.proc_rx.try_recv() {
                match event {
                    ProcEvent::Output { tab_id, bytes } => {
                        self.log(LogLevel::Trace, format!("tab={tab_id} output bytes={}", bytes.len()));
                        if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                            session.parser.process(&bytes);
                        } else {
                            let text = String::from_utf8_lossy(&bytes).to_string();
                            for line in text.lines() {
                                self.connections.append_line(tab_id, line.to_string());
                            }
                        }
                        self.maybe_record_gui_smoke_success(tab_id, &bytes);
                    }
                    ProcEvent::Error { tab_id, error } => {
                        self.log_error(format!("tab={tab_id} process error: {error}"));
                        self.connections
                            .append_line(tab_id, format!("[error] {error}"));
                    }
                    ProcEvent::Exited { tab_id, code } => {
                        self.log_info(format!("tab={tab_id} process exited with code {code}"));
                        self.connections
                            .append_line(tab_id, format!("[exit] code={code}"));
                        self.connections.set_running(tab_id, false);
                    }
                }
            }

            let mut exited: Vec<(u64, i32)> = Vec::new();
            let mut wait_errors: Vec<String> = Vec::new();
            for (tab_id, session) in &mut self.pty_sessions {
                match session.child.try_wait() {
                    Ok(Some(status)) => {
                        exited.push((*tab_id, status.exit_code() as i32));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        wait_errors.push(format!("tab={tab_id} wait error: {err}"));
                        self.connections
                            .append_line(*tab_id, format!("[wait error] {err}"));
                        exited.push((*tab_id, -1));
                    }
                }
            }
            for (tab_id, session) in &mut self.pipe_sessions {
                match session.child.try_wait() {
                    Ok(Some(status)) => {
                        exited.push((*tab_id, status.code().unwrap_or(-1)));
                    }
                    Ok(None) => {}
                    Err(err) => {
                        wait_errors.push(format!("tab={tab_id} wait error: {err}"));
                        self.connections
                            .append_line(*tab_id, format!("[wait error] {err}"));
                        exited.push((*tab_id, -1));
                    }
                }
            }
            for message in wait_errors {
                self.log_error(message);
            }

            for (tab_id, code) in exited {
                self.pty_sessions.remove(&tab_id);
                self.pipe_sessions.remove(&tab_id);
                let _ = self.proc_tx.send(ProcEvent::Exited { tab_id, code });
            }
        }

        fn close_connection_tab(&mut self, tab_id: u64) {
            if let Some(mut session) = self.pty_sessions.remove(&tab_id) {
                let _ = session.child.kill();
                let _ = session.child.wait();
            }
            if let Some(mut session) = self.pipe_sessions.remove(&tab_id) {
                let _ = session.child.kill();
                let _ = session.child.wait();
            }
            self.connections.close(tab_id);
            self.log_info(format!("closed connection tab id={tab_id}"));
        }

        fn send_raw_bytes_to_connection_tab(&mut self, tab_id: u64, payload: &[u8]) {
            let mut write_error: Option<String> = None;
            let Some(session) = self.pty_sessions.get(&tab_id) else {
                if let Some(pipe) = self.pipe_sessions.get(&tab_id) {
                    if let Ok(mut stdin) = pipe.stdin.lock() {
                        if let Err(err) = stdin.write_all(payload) {
                            write_error = Some(format!("tab={tab_id} write error: {err}"));
                        }
                    }
                }
                if let Some(message) = write_error {
                    self.log_error(message);
                }
                return;
            };
            let Ok(mut stdin) = session.writer.lock() else {
                return;
            };
            if let Err(err) = stdin.write_all(payload) {
                write_error = Some(format!("tab={tab_id} write error: {err}"));
            }
            drop(stdin);
            if let Some(message) = write_error {
                self.log_error(message);
            }
        }

        fn forward_terminal_key_input(&mut self, ctx: &egui::Context, tab_id: u64) {
            let events = ctx.input(|i| i.raw.events.clone());
            for event in events {
                if let Some(payload) = terminal_event_payload(&event) {
                    self.send_raw_bytes_to_connection_tab(tab_id, &payload);
                }
            }
        }

        fn toggle_favorite_selected(&mut self) -> Result<()> {
            let Some(instance_id) = self.selected_instance().map(|i| i.instance_id.clone()) else {
                return Err(AppError::NotFound("Select an instance first".to_string()));
            };

            let enabled = self.config.toggle_favorite(
                &self.account_scope(),
                &self.region_scope(),
                &instance_id,
            );
            self.config.save()?;
            self.message = format!(
                "Favorite {}: {}",
                if enabled { "enabled" } else { "disabled" },
                instance_id
            );
            self.log_info(self.message.clone());
            Ok(())
        }

        fn save_current_filter(&mut self) -> Result<()> {
            let name = self.save_filter_name.trim();
            if name.is_empty() {
                return Err(AppError::InvalidArgument(
                    "Filter name cannot be empty".to_string(),
                ));
            }

            let states = states_from_state_filter(&self.selected_state_filter);
            let (include_terms, exclude_terms) = search_terms_from_rules(&self.search_rules);

            self.config.upsert_saved_filter(
                &self.account_scope(),
                &self.region_scope(),
                SavedFilter {
                    name: name.to_string(),
                    include_terms,
                    exclude_terms,
                    states,
                    only_ssm_managed: self.only_ssm,
                },
            );
            self.config.save()?;
            self.message = format!("Saved filter: {name}");
            self.log_info(self.message.clone());
            Ok(())
        }

        fn apply_saved_filter(&mut self) -> Result<()> {
            let name = self.selected_saved_filter.trim().to_string();
            if name.is_empty() {
                return Err(AppError::InvalidArgument(
                    "Select a saved filter first".to_string(),
                ));
            }

            let scope_filters = self
                .config
                .saved_filters_for_scope(&self.account_scope(), &self.region_scope());
            let Some(saved) = scope_filters
                .into_iter()
                .find(|f| f.name.eq_ignore_ascii_case(&name))
            else {
                return Err(AppError::NotFound(format!(
                    "Saved filter not found: {name}"
                )));
            };

            self.search_rules = rules_from_search_terms(&saved.include_terms, &saved.exclude_terms);
            self.selected_state_filter = state_filter_from_saved_states(&saved.states);
            self.only_ssm = saved.only_ssm_managed;
            self.apply_filters();
            self.message = format!("Applied saved filter: {name}");
            self.log_info(self.message.clone());
            Ok(())
        }

        fn run_diagnostics(&mut self) {
            if let Some(context) = &self.context {
                let report = run_diagnostics(context, &self.dependencies, &[], &self.config);
                self.diagnostics = format!(
                    "mode={}\nprofile={}\nauth={}\naws_cli={}\nssm_plugin={}\nec2_check={:?}\nssm_check={:?}",
                    report.mode,
                    report.profile,
                    report.auth_status,
                    report.aws_cli_found,
                    report.ssm_plugin_found,
                    report.ec2_check,
                    report.ssm_check,
                );
                self.log_info("diagnostics completed");
            }
        }

        fn render_inventory_panel(&mut self, ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Instances: {} filtered / {} total",
                    self.filtered.len(),
                    self.inventory.instances.len()
                ));
            });

            egui::ScrollArea::both().show(ui, |ui| {
                egui::Grid::new("instance_grid")
                    .striped(true)
                    .show(ui, |ui| {
                        ui.add_sized(
                            [COL_FAV_W, 18.0],
                            egui::Label::new(egui::RichText::new("Fav").strong()),
                        );
                        ui.add_sized(
                            [COL_INSTANCE_W, 18.0],
                            egui::Label::new(egui::RichText::new("InstanceId").strong()),
                        );
                        ui.add_sized(
                            [COL_NAME_W, 18.0],
                            egui::Label::new(egui::RichText::new("Name").strong()),
                        );
                        ui.add_sized(
                            [COL_STATE_W, 18.0],
                            egui::Label::new(egui::RichText::new("State").strong()),
                        );
                        ui.add_sized(
                            [COL_SSM_W, 18.0],
                            egui::Label::new(egui::RichText::new("SSM").strong()),
                        );
                        ui.add_sized(
                            [COL_IP_W, 18.0],
                            egui::Label::new(egui::RichText::new("Private IP").strong()),
                        );
                        ui.add_sized(
                            [COL_ENV_W, 18.0],
                            egui::Label::new(egui::RichText::new("Env").strong()),
                        );
                        ui.add_sized(
                            [COL_APP_W, 18.0],
                            egui::Label::new(egui::RichText::new("App/Service").strong()),
                        );
                        ui.end_row();

                        let account_scope = self.account_scope();
                        let region_scope = self.region_scope();
                        let mut pending_connect: Option<String> = None;

                        for instance in &self.filtered {
                            let is_fav = self.config.is_favorite(
                                &account_scope,
                                &region_scope,
                                &instance.instance_id,
                            );
                            let selected = self.selected_instance_id == instance.instance_id;
                            let mut row_clicked = false;
                            let mut row_double_clicked = false;
                            let mut row_hovered = false;
                            let mut quick_connect_clicked = false;

                            let resp_fav = ui.add_sized(
                                [COL_FAV_W, 18.0],
                                egui::Label::new(if is_fav { "*" } else { "" })
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_fav.clicked();
                            row_double_clicked |= resp_fav.double_clicked();
                            row_hovered |= resp_fav.hovered();

                            let resp_id = ui.add_sized(
                                [COL_INSTANCE_W, 18.0],
                                egui::SelectableLabel::new(selected, instance.instance_id.clone()),
                            );
                            row_clicked |= resp_id.clicked();
                            row_double_clicked |= resp_id.double_clicked();
                            row_hovered |= resp_id.hovered();

                            let resp_name = ui.add_sized(
                                [COL_NAME_W, 18.0],
                                egui::Label::new(instance.name.clone().unwrap_or_default())
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_name.clicked();
                            row_double_clicked |= resp_name.double_clicked();
                            row_hovered |= resp_name.hovered();

                            let resp_state = ui.add_sized(
                                [COL_STATE_W, 18.0],
                                egui::Label::new(instance.state.clone())
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_state.clicked();
                            row_double_clicked |= resp_state.double_clicked();
                            row_hovered |= resp_state.hovered();

                            let resp_ssm = ui.add_sized(
                                [COL_SSM_W, 18.0],
                                egui::Label::new(if instance.ssm_managed {
                                    instance
                                        .ssm_ping
                                        .clone()
                                        .unwrap_or_else(|| "Managed".to_string())
                                } else {
                                    "No".to_string()
                                })
                                .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_ssm.clicked();
                            row_double_clicked |= resp_ssm.double_clicked();
                            row_hovered |= resp_ssm.hovered();

                            let resp_ip = ui.add_sized(
                                [COL_IP_W, 18.0],
                                egui::Label::new(instance.private_ip.clone().unwrap_or_default())
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_ip.clicked();
                            row_double_clicked |= resp_ip.double_clicked();
                            row_hovered |= resp_ip.hovered();

                            let resp_env = ui.add_sized(
                                [COL_ENV_W, 18.0],
                                egui::Label::new(instance.env.clone().unwrap_or_default())
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_env.clicked();
                            row_double_clicked |= resp_env.double_clicked();
                            row_hovered |= resp_env.hovered();

                            let resp_app = ui.add_sized(
                                [COL_APP_W, 18.0],
                                egui::Label::new(instance.app_service.clone().unwrap_or_default())
                                    .sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_app.clicked();
                            row_double_clicked |= resp_app.double_clicked();
                            row_hovered |= resp_app.hovered();

                            let row_response = resp_fav
                                .clone()
                                .union(resp_id.clone())
                                .union(resp_name.clone())
                                .union(resp_state.clone())
                                .union(resp_ssm.clone())
                                .union(resp_ip.clone())
                                .union(resp_env.clone())
                                .union(resp_app.clone());

                            row_response.context_menu(|ui| {
                                if ui.button("Quick Connect").clicked() {
                                    quick_connect_clicked = true;
                                    ui.close_menu();
                                }
                            });

                            if row_hovered {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }

                            let row_rect = resp_fav
                                .rect
                                .union(resp_id.rect)
                                .union(resp_name.rect)
                                .union(resp_state.rect)
                                .union(resp_ssm.rect)
                                .union(resp_ip.rect)
                                .union(resp_env.rect)
                                .union(resp_app.rect);

                            if selected || row_hovered {
                                let color = if selected {
                                    egui::Color32::from_rgba_unmultiplied(70, 110, 170, 55)
                                } else {
                                    egui::Color32::from_rgba_unmultiplied(90, 90, 90, 35)
                                };
                                ui.painter().rect_filled(row_rect, 0.0, color);
                            }

                            let action = resolve_row_action(
                                row_clicked,
                                row_double_clicked,
                                quick_connect_clicked,
                            );

                            if action.select {
                                self.selected_instance_id = instance.instance_id.clone();
                            }
                            if action.connect {
                                pending_connect = Some(instance.instance_id.clone());
                            }

                            ui.end_row();
                        }

                        if let Some(instance_id) = pending_connect {
                            self.selected_instance_id = instance_id;
                            if let Err(err) = self.connect_selected() {
                                self.message = format!("error: {err}");
                                self.log_error(self.message.clone());
                            }
                            self.main_tab = MainTab::Connections;
                        }
                    });
            });
        }

        fn render_connections_panel(&mut self, ui: &mut egui::Ui) {
            let tabs_snapshot: Vec<(u64, String, bool)> = self
                .connections
                .tabs()
                .iter()
                .map(|t| (t.id, t.title.clone(), t.running))
                .collect();

            if tabs_snapshot.is_empty() {
                ui.label("No active connections. Select an instance and click Connect.");
                return;
            }

            let mut to_select: Option<u64> = None;
            let mut to_close: Option<u64> = None;

            ui.horizontal_wrapped(|ui| {
                for (id, title, running) in tabs_snapshot {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            let prefix = if running { "" } else { "[done] " };
                            let selected = self.connections.selected() == Some(id);
                            if ui
                                .selectable_label(
                                    selected,
                                    format!("{}{}", prefix, truncate(&title, 28)),
                                )
                                .clicked()
                            {
                                to_select = Some(id);
                            }
                            if ui.small_button("x").clicked() {
                                to_close = Some(id);
                            }
                        });
                    });
                }
            });

            if let Some(id) = to_select {
                self.connections.select(id);
            }
            if let Some(id) = to_close {
                self.close_connection_tab(id);
            }

            ui.separator();

            if let Some(tab) = self.connections.selected_ref().cloned() {
                let private_ip = find_instance(&self.inventory.instances, &tab.instance_id)
                    .and_then(|i| i.private_ip.clone())
                    .unwrap_or_else(|| "-".to_string());
                ui.monospace(format_connection_summary_line(
                    &tab.title,
                    &tab.instance_id,
                    &private_ip,
                    tab.running,
                ));
                ui.separator();

                let show_cursor = ui.input(|i| ((i.time * 2.0) as i64) % 2 == 0);
                let terminal_text = self
                    .pty_sessions
                    .get(&tab.id)
                    .map(|s| terminal_text_with_cursor(s.parser.screen(), show_cursor))
                    .unwrap_or_else(|| tab.lines.join("\n"));

                let terminal_response = egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(terminal_text).monospace())
                            .sense(egui::Sense::click()),
                    )
                });
                let terminal_focus_id = ui.make_persistent_id(("terminal_focus", tab.id));
                let terminal_focus_response = ui.interact(
                    terminal_response.inner.rect,
                    terminal_focus_id,
                    egui::Sense::click(),
                );
                if terminal_focus_response.clicked() {
                    terminal_focus_response.request_focus();
                }
                if terminal_focus_response.has_focus() {
                    self.forward_terminal_key_input(ui.ctx(), tab.id);
                }
            }
        }

        fn render_log_panel(&mut self, ui: &mut egui::Ui) {
            ui.horizontal(|ui| {
                ui.label("Application log");
                ui.separator();
                if ui.button("Clear").clicked() {
                    self.logs.clear();
                    self.log_info("log cleared");
                }
                ui.separator();
                if ui.button("Low").clicked() {
                    self.log_filters.set_verbosity_low();
                }
                if ui.button("Medium").clicked() {
                    self.log_filters.set_verbosity_medium();
                }
                if ui.button("High").clicked() {
                    self.log_filters.set_verbosity_high();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.log_filters.trace, "TRACE");
                    ui.checkbox(&mut self.log_filters.debug, "DEBUG");
                    ui.checkbox(&mut self.log_filters.info, "INFO");
                    ui.checkbox(&mut self.log_filters.warn, "WARN");
                    ui.checkbox(&mut self.log_filters.error, "ERROR");
                });
            });
            ui.separator();

            let matching_count = self
                .logs
                .iter()
                .filter(|entry| self.log_filters.includes(entry.level))
                .count();
            ui.label(format!(
                "Showing {matching_count} / {} log lines",
                self.logs.len()
            ));
            ui.separator();

            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for entry in self
                        .logs
                        .iter()
                        .filter(|entry| self.log_filters.includes(entry.level))
                    {
                        let color = match entry.level {
                            LogLevel::Error => egui::Color32::RED,
                            LogLevel::Warn => egui::Color32::YELLOW,
                            LogLevel::Info => egui::Color32::LIGHT_GREEN,
                            LogLevel::Debug => egui::Color32::LIGHT_BLUE,
                            LogLevel::Trace => egui::Color32::GRAY,
                        };
                        ui.colored_label(
                            color,
                            format!("[{}] {}", entry.level.as_str(), entry.message),
                        );
                    }
                });
        }
    }

    impl eframe::App for Ec2GuiApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            let update_result = panic::catch_unwind(AssertUnwindSafe(|| {
                self.poll_profile_choice_changes();
                self.poll_connection_events();
                if self.main_tab == MainTab::Connections && !self.pty_sessions.is_empty() {
                    ctx.request_repaint_after(CURSOR_BLINK_INTERVAL);
                }
                if self.gui_smoke_should_close {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }

                egui::TopBottomPanel::top("top").show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("EC2 + SSM Instance Explorer");
                        ui.separator();
                        ui.label(format!("Mode: {}", self.options.mode.as_str()));
                        if let Some(c) = &self.context {
                            ui.label(format!("Profile: {}", c.profile));
                            ui.label(format!(
                                "Account: {}",
                                c.account_id.as_deref().unwrap_or("unknown")
                            ));
                            ui.label(format!("Region: {}", c.region));
                            ui.label(format!("Auth: {}", c.auth_status));
                        }
                    });

                    ui.horizontal(|ui| {
                        if ui
                            .selectable_label(self.main_tab == MainTab::Inventory, "Inventory")
                            .clicked()
                        {
                            self.main_tab = MainTab::Inventory;
                        }
                        if ui
                            .selectable_label(
                                self.main_tab == MainTab::Connections,
                                format!("Connections ({})", self.connections.tabs().len()),
                            )
                            .clicked()
                        {
                            self.main_tab = MainTab::Connections;
                        }
                        if ui
                            .selectable_label(self.main_tab == MainTab::Log, "Log")
                            .clicked()
                        {
                            self.main_tab = MainTab::Log;
                        }
                    });

                    if !self.message.is_empty() {
                        ui.label(self.message.clone());
                    }
                });

                egui::SidePanel::left("controls")
                    .resizable(true)
                    .show(ctx, |ui| {
                    ui.heading("Controls");

                    if ui.button("Refresh Inventory").clicked() {
                        if let Err(err) = self.refresh_context_and_inventory(true) {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }
                    let before_region = self.options.region.clone();
                    let context_region = self.context.as_ref().map(|c| c.region.as_str());
                    let selected_region_text =
                        selected_region_label(self.options.region.as_deref(), context_region);
                    egui::ComboBox::from_id_salt("region_selector_combo")
                        .selected_text(selected_region_text)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.options.region, None, AWS_REGION_AUTO);
                            for region in AWS_REGIONS {
                                ui.selectable_value(
                                    &mut self.options.region,
                                    Some((*region).to_string()),
                                    *region,
                                );
                            }
                        });
                    if self.options.region != before_region {
                        self.config.default_region = self.options.region.clone();
                        if let Err(err) = self.config.save() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        } else {
                            self.log_info(format!(
                                "region selection changed to {}",
                                self.options
                                    .region
                                    .clone()
                                    .unwrap_or_else(|| AWS_REGION_AUTO.to_string())
                            ));
                        }
                        if let Err(err) = self.refresh_context_and_inventory(true) {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Terminal");
                        if ui.small_button("Rescan").clicked() {
                            self.terminals = discover_terminals();
                            let prior = self.selected_terminal_id.clone();
                            self.selected_terminal_id =
                                initial_terminal_id(&self.config, &self.terminals);
                            if self.selected_terminal_id != prior {
                                self.log_info(format!(
                                    "terminal selection updated to {} after rescan",
                                    if self.selected_terminal_id.is_empty() {
                                        "(none)".to_string()
                                    } else {
                                        self.selected_terminal_id.clone()
                                    }
                                ));
                            }
                        }
                    });
                    let before_terminal_id = self.selected_terminal_id.clone();
                    egui::ComboBox::from_id_salt("terminal_combo")
                        .selected_text(
                            self.selected_terminal()
                                .map(|t| format!("{} ({})", t.display_name, t.id))
                                .unwrap_or_else(|| "(none detected)".to_string()),
                        )
                        .show_ui(ui, |ui| {
                            for terminal in &self.terminals {
                                ui.selectable_value(
                                    &mut self.selected_terminal_id,
                                    terminal.id.clone(),
                                    format!("{} ({})", terminal.display_name, terminal.id),
                                );
                            }
                        });
                    if self.selected_terminal_id != before_terminal_id {
                        if self.selected_terminal_id.is_empty() {
                            self.config.default_terminal = None;
                        } else {
                            self.config.default_terminal = Some(self.selected_terminal_id.clone());
                        }
                        if let Err(err) = self.config.save() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        } else if let Some(selected) = self.selected_terminal() {
                            self.message = format!(
                                "Default terminal set to {} ({})",
                                selected.display_name, selected.id
                            );
                            self.log_info(self.message.clone());
                        }
                    }

                    ui.horizontal(|ui| {
                        ui.label("Search Rules");
                        if ui.button("+").on_hover_text("Add search rule").clicked() {
                            self.search_rules.push(SearchRuleInput::default());
                            self.apply_filters();
                        }
                    });

                    let mut remove_rule_idx: Option<usize> = None;
                    let mut rules_changed = false;
                    for (idx, rule) in self.search_rules.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt(format!("search_rule_kind_{idx}"))
                                .selected_text(match rule.kind {
                                    SearchRuleKind::Include => "Include",
                                    SearchRuleKind::Exclude => "Exclude",
                                })
                                .show_ui(ui, |ui| {
                                    rules_changed |= ui
                                        .selectable_value(
                                            &mut rule.kind,
                                            SearchRuleKind::Include,
                                            "Include",
                                        )
                                        .changed();
                                    rules_changed |= ui
                                        .selectable_value(
                                            &mut rule.kind,
                                            SearchRuleKind::Exclude,
                                            "Exclude",
                                        )
                                        .changed();
                                });
                            if ui.text_edit_singleline(&mut rule.term).changed() {
                                rules_changed = true;
                            }
                            if ui.small_button("-").clicked() {
                                remove_rule_idx = Some(idx);
                            }
                        });
                    }
                    if let Some(idx) = remove_rule_idx {
                        self.search_rules.remove(idx);
                        if self.search_rules.is_empty() {
                            self.search_rules.push(SearchRuleInput::default());
                        }
                        rules_changed = true;
                    }
                    if rules_changed {
                        self.apply_filters();
                    }

                    ui.horizontal(|ui| {
                        ui.label("States");
                        let before = self.selected_state_filter.clone();
                        egui::ComboBox::from_id_salt("state_filter_combo")
                            .selected_text(if self.selected_state_filter.is_empty() {
                                "No filter".to_string()
                            } else {
                                self.selected_state_filter.clone()
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_state_filter,
                                    STATE_FILTER_NONE.to_string(),
                                    "No filter",
                                );
                                ui.selectable_value(
                                    &mut self.selected_state_filter,
                                    STATE_FILTER_RUNNING.to_string(),
                                    "running",
                                );
                                ui.selectable_value(
                                    &mut self.selected_state_filter,
                                    STATE_FILTER_STOPPED.to_string(),
                                    "stopped",
                                );
                                ui.selectable_value(
                                    &mut self.selected_state_filter,
                                    STATE_FILTER_TERMINATED.to_string(),
                                    "terminated",
                                );
                            });
                        if self.selected_state_filter != before {
                            self.apply_filters();
                        }
                    });

                    if ui
                        .checkbox(&mut self.only_ssm, "Only SSM-managed")
                        .changed()
                    {
                        self.apply_filters();
                    }

                    ui.separator();
                    ui.label("Selected Instance ID");
                    ui.text_edit_singleline(&mut self.selected_instance_id);

                    if ui.button("Connect").clicked() {
                        if let Err(err) = self.connect_selected() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }

                    if ui.button("Toggle Favorite").clicked() {
                        if let Err(err) = self.toggle_favorite_selected() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }

                    ui.horizontal(|ui| {
                        ui.label("Local Port");
                        ui.add(egui::DragValue::new(&mut self.local_port).range(1..=65535));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Remote Port");
                        ui.add(egui::DragValue::new(&mut self.remote_port).range(1..=65535));
                    });
                    if ui.button("Port Forward").clicked() {
                        if let Err(err) = self.port_forward_selected() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }

                    ui.separator();
                    ui.label("Saved Filters");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.save_filter_name);
                        if ui.button("Save Current").clicked() {
                            if let Err(err) = self.save_current_filter() {
                                self.message = format!("error: {err}");
                                self.log_error(self.message.clone());
                            }
                        }
                    });

                    let scope_filters = self
                        .config
                        .saved_filters_for_scope(&self.account_scope(), &self.region_scope());

                    egui::ComboBox::from_id_salt("saved_filter_combo")
                        .selected_text(if self.selected_saved_filter.is_empty() {
                            "(choose)".to_string()
                        } else {
                            self.selected_saved_filter.clone()
                        })
                        .show_ui(ui, |ui| {
                            for saved in scope_filters {
                                ui.selectable_value(
                                    &mut self.selected_saved_filter,
                                    saved.name.clone(),
                                    saved.name,
                                );
                            }
                        });

                    if ui.button("Apply Saved").clicked() {
                        if let Err(err) = self.apply_saved_filter() {
                            self.message = format!("error: {err}");
                            self.log_error(self.message.clone());
                        }
                    }

                    if ui.button("Run Diagnostics").clicked() {
                        self.run_diagnostics();
                    }
                    if !self.diagnostics.is_empty() {
                        ui.separator();
                        ui.code(self.diagnostics.clone());
                    }

                    ui.separator();
                    ui.label(format!(
                        "aws CLI: {} | ssm plugin: {} | terminals: {}",
                        self.dependencies.aws_cli_found,
                        self.dependencies.ssm_plugin_found,
                        self.terminals.len()
                    ));
                    });

                egui::CentralPanel::default().show(ctx, |ui| match self.main_tab {
                    MainTab::Inventory => self.render_inventory_panel(ui),
                    MainTab::Connections => self.render_connections_panel(ui),
                    MainTab::Log => self.render_log_panel(ui),
                });
            }));

            if let Err(payload) = update_result {
                let panic_message = format!(
                    "UI panic recovered: {}",
                    panic_payload_to_string(payload.as_ref())
                );
                append_panic_log_entry(&panic_message);
                self.message = panic_message.clone();
                self.log_error(panic_message);
                self.main_tab = MainTab::Log;
            }
        }
    }

    fn shell_plan(terminal: Option<&TerminalOption>) -> (String, Vec<String>) {
        if cfg!(windows) {
            let fallback = || ("cmd".to_string(), vec!["/Q".to_string(), "/K".to_string()]);
            match terminal.map(|t| t.id.as_str()) {
                Some("pwsh") => (
                    terminal
                        .map(|t| t.program.clone())
                        .unwrap_or_else(|| "pwsh".to_string()),
                    vec!["-NoExit".to_string()],
                ),
                Some("powershell") => (
                    terminal
                        .map(|t| t.program.clone())
                        .unwrap_or_else(|| "powershell".to_string()),
                    vec!["-NoExit".to_string()],
                ),
                Some("cmd") => ("cmd".to_string(), vec!["/Q".to_string(), "/K".to_string()]),
                Some("wsl") => (
                    terminal
                        .map(|t| t.program.clone())
                        .unwrap_or_else(|| "wsl".to_string()),
                    vec!["--".to_string(), "bash".to_string()],
                ),
                _ => fallback(),
            }
        } else {
            ("/bin/bash".to_string(), vec!["-i".to_string()])
        }
    }

    #[cfg(test)]
    fn terminate_child(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn resolve_row_action(
        row_clicked: bool,
        row_double_clicked: bool,
        quick_connect_clicked: bool,
    ) -> RowAction {
        RowAction {
            select: row_clicked || row_double_clicked || quick_connect_clicked,
            connect: row_double_clicked || quick_connect_clicked,
        }
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn profile_choice_mtime(path: Option<&std::path::Path>) -> Option<SystemTime> {
        let path = path?;
        fs::metadata(path).ok()?.modified().ok()
    }

    #[cfg(test)]
    fn profile_choice_changed(
        previous: Option<SystemTime>,
        current: Option<SystemTime>,
    ) -> bool {
        previous != current
    }

    fn profile_change_debounce_elapsed(
        started_at: Option<SystemTime>,
        now: SystemTime,
        debounce: Duration,
    ) -> bool {
        let Some(started_at) = started_at else {
            return false;
        };
        match now.duration_since(started_at) {
            Ok(elapsed) => elapsed >= debounce,
            Err(_) => false,
        }
    }

    fn parse_bool_env(raw: Option<&str>, default: bool) -> bool {
        let Some(value) = raw else {
            return default;
        };
        match value.trim().to_ascii_lowercase().as_str() {
            "" => default,
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        }
    }

    fn should_try_pty_from_env(value: Option<&str>) -> bool {
        !parse_bool_env(value, false)
    }

    #[cfg(target_os = "windows")]
    fn should_try_pty() -> bool {
        should_try_pty_from_env(std::env::var("EC2_MANAGER_GUI_FORCE_PIPE").ok().as_deref())
    }

    fn gui_smoke_config_from_env() -> Option<GuiSmokeConfig> {
        let marker_path = std::env::var_os(GUI_SMOKE_MARKER_ENV)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())?;

        let expected_text = std::env::var(GUI_SMOKE_EXPECTED_TEXT_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "[SIM MODE] session open for".to_string());

        let exit_on_marker = parse_bool_env(
            std::env::var(GUI_SMOKE_EXIT_ON_MARKER_ENV).ok().as_deref(),
            false,
        );
        let auto_connect = parse_bool_env(
            std::env::var(GUI_SMOKE_AUTO_CONNECT_ENV).ok().as_deref(),
            true,
        );

        Some(GuiSmokeConfig {
            marker_path,
            expected_text,
            exit_on_marker,
            auto_connect,
        })
    }

    fn gui_smoke_match_in_bytes(expected_text: &str, bytes: &[u8]) -> bool {
        if expected_text.trim().is_empty() {
            return false;
        }
        String::from_utf8_lossy(bytes).contains(expected_text)
    }

    fn gui_smoke_marker_payload(tab_id: u64, expected_text: &str) -> String {
        format!("PASS\ntab_id={tab_id}\nexpected={expected_text}\n")
    }

    fn write_gui_smoke_marker(path: &std::path::Path, tab_id: u64, expected_text: &str) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, gui_smoke_marker_payload(tab_id, expected_text))
    }

    fn format_connection_summary_line(
        title: &str,
        instance_id: &str,
        private_ip: &str,
        running: bool,
    ) -> String {
        let status = if running { "Running" } else { "Closed" };
        format!(
            "Instance: {title} ({instance_id})\tPrivate IP: {private_ip}\tStatus: {status}"
        )
    }

    fn should_auto_send_prefilled_command(dry_run: bool) -> bool {
        !dry_run
    }

    fn terminal_text_with_cursor(screen: &vt100::Screen, show_cursor: bool) -> String {
        let (rows, cols) = screen.size();
        let mut row_texts: Vec<Vec<char>> = screen
            .rows(0, cols)
            .map(|line| line.chars().collect::<Vec<char>>())
            .collect();
        if row_texts.len() < rows as usize {
            row_texts.resize(rows as usize, Vec::new());
        }

        if show_cursor {
            let (row, col) = screen.cursor_position();
            let row_idx = row as usize;
            let col_idx = col as usize;
            if row_idx >= row_texts.len() {
                row_texts.resize(row_idx + 1, Vec::new());
            }
            let line = &mut row_texts[row_idx];
            while line.len() < col_idx {
                line.push(' ');
            }
            if col_idx < line.len() {
                line[col_idx] = '|';
            } else {
                line.push('|');
            }
        }

        row_texts
            .into_iter()
            .map(|chars| chars.into_iter().collect::<String>())
            .collect::<Vec<String>>()
            .join("\n")
    }

    fn terminal_event_payload(event: &egui::Event) -> Option<Vec<u8>> {
        match event {
            egui::Event::Text(text) => Some(text.as_bytes().to_vec()),
            egui::Event::Paste(text) => Some(text.as_bytes().to_vec()),
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match key {
                egui::Key::Enter => Some(b"\r".to_vec()),
                egui::Key::Backspace => Some(vec![0x7f]),
                egui::Key::Tab => Some(b"\t".to_vec()),
                egui::Key::Escape => Some(vec![0x1b]),
                egui::Key::ArrowUp => Some(b"\x1b[A".to_vec()),
                egui::Key::ArrowDown => Some(b"\x1b[B".to_vec()),
                egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
                egui::Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
                egui::Key::C if modifiers.ctrl => Some(vec![0x03]),
                egui::Key::D if modifiers.ctrl => Some(vec![0x04]),
                egui::Key::L if modifiers.ctrl => Some(vec![0x0c]),
                _ if !modifiers.ctrl && !modifiers.command && !modifiers.alt => {
                    key_ascii_fallback(*key, modifiers.shift).map(|c| vec![c])
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn key_ascii_fallback(key: egui::Key, shift: bool) -> Option<u8> {
        let letter = match key {
            egui::Key::A => Some(b'a'),
            egui::Key::B => Some(b'b'),
            egui::Key::C => Some(b'c'),
            egui::Key::D => Some(b'd'),
            egui::Key::E => Some(b'e'),
            egui::Key::F => Some(b'f'),
            egui::Key::G => Some(b'g'),
            egui::Key::H => Some(b'h'),
            egui::Key::I => Some(b'i'),
            egui::Key::J => Some(b'j'),
            egui::Key::K => Some(b'k'),
            egui::Key::L => Some(b'l'),
            egui::Key::M => Some(b'm'),
            egui::Key::N => Some(b'n'),
            egui::Key::O => Some(b'o'),
            egui::Key::P => Some(b'p'),
            egui::Key::Q => Some(b'q'),
            egui::Key::R => Some(b'r'),
            egui::Key::S => Some(b's'),
            egui::Key::T => Some(b't'),
            egui::Key::U => Some(b'u'),
            egui::Key::V => Some(b'v'),
            egui::Key::W => Some(b'w'),
            egui::Key::X => Some(b'x'),
            egui::Key::Y => Some(b'y'),
            egui::Key::Z => Some(b'z'),
            egui::Key::Num0 => Some(b'0'),
            egui::Key::Num1 => Some(b'1'),
            egui::Key::Num2 => Some(b'2'),
            egui::Key::Num3 => Some(b'3'),
            egui::Key::Num4 => Some(b'4'),
            egui::Key::Num5 => Some(b'5'),
            egui::Key::Num6 => Some(b'6'),
            egui::Key::Num7 => Some(b'7'),
            egui::Key::Num8 => Some(b'8'),
            egui::Key::Num9 => Some(b'9'),
            egui::Key::Space => Some(b' '),
            _ => None,
        }?;
        if shift && letter.is_ascii_lowercase() {
            Some(letter.to_ascii_uppercase())
        } else {
            Some(letter)
        }
    }

    fn initial_terminal_id(config: &AppConfig, terminals: &[TerminalOption]) -> String {
        pick_default_terminal(config, terminals)
            .or_else(|| terminals.first().cloned())
            .map(|t| t.id)
            .unwrap_or_default()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn default_window_size_is_large_enough() {
            assert!(GUI_DEFAULT_WIDTH >= 1600.0);
            assert!(GUI_DEFAULT_HEIGHT >= 900.0);
            assert!(GUI_MIN_WIDTH >= 1200.0);
            assert!(GUI_MIN_HEIGHT >= 700.0);
        }

        #[test]
        fn shell_plan_has_program() {
            let (prog, args) = shell_plan(None);
            assert!(!prog.is_empty());
            assert!(!args.is_empty());
        }

        #[test]
        #[cfg(windows)]
        fn shell_plan_falls_back_for_msys2_bash() {
            let terminal = TerminalOption {
                id: "msys2-bash".to_string(),
                display_name: "MSYS2 Bash".to_string(),
                kind: ec2_manager::models::TerminalKind::GitBash,
                program: "C:\\msys64\\usr\\bin\\bash.exe".to_string(),
            };

            let (prog, args) = shell_plan(Some(&terminal));
            assert_eq!(prog, "cmd");
            assert!(args.iter().any(|a| a == "/K"));
        }

        #[test]
        fn terminate_child_reaps_process() {
            let (program, args) = if cfg!(windows) {
                (
                    "cmd",
                    vec!["/C".to_string(), "ping -n 10 127.0.0.1 >NUL".to_string()],
                )
            } else {
                ("/bin/sh", vec!["-c".to_string(), "sleep 35".to_string()])
            };

            let mut child = Command::new(program)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn process");

            terminate_child(&mut child);
            assert!(child.try_wait().expect("process state").is_some());
        }

        #[test]
        fn quick_connect_row_action_connects_and_selects() {
            let action = resolve_row_action(false, false, true);
            assert_eq!(
                action,
                RowAction {
                    select: true,
                    connect: true
                }
            );
        }

        #[test]
        fn single_click_row_action_selects_only() {
            let action = resolve_row_action(true, false, false);
            assert_eq!(
                action,
                RowAction {
                    select: true,
                    connect: false
                }
            );
        }

        #[test]
        fn open_connection_tab_dry_run_does_not_spawn_child() {
            let mut app = Ec2GuiApp::new(GuiOptions {
                mode: Mode::Sim,
                region: None,
                dry_run: true,
            });
            let context = AwsContext {
                mode: Mode::Sim,
                profile: "sim-profile".to_string(),
                account_id: Some("000000000000".to_string()),
                arn: None,
                user_id: None,
                region: "us-east-1".to_string(),
                auth_status: AuthStatus::Ok,
            };

            app.open_connection_tab(
                "api-a".to_string(),
                "i-sim0001".to_string(),
                "echo hi".to_string(),
                &context,
            )
            .expect("dry-run open should succeed");

            assert!(app.pty_sessions.is_empty());
            let selected = app
                .connections
                .selected_ref()
                .expect("tab should be selected");
            assert!(!selected.running);
            assert!(selected.lines.iter().any(|line| line.contains("[dry-run]")));
        }

        #[test]
        fn sim_mode_open_connection_tab_spawns_interactive_terminal_session() {
            let mut app = Ec2GuiApp::new(GuiOptions {
                mode: Mode::Sim,
                region: None,
                dry_run: false,
            });
            let context = AwsContext {
                mode: Mode::Sim,
                profile: "sim-profile".to_string(),
                account_id: Some("000000000000".to_string()),
                arn: None,
                user_id: None,
                region: "us-east-1".to_string(),
                auth_status: AuthStatus::Ok,
            };

            let open_result = app.open_connection_tab(
                "api-a".to_string(),
                "i-sim0001".to_string(),
                "echo terminal-ok".to_string(),
                &context,
            );
            if let Err(AppError::Parse(message)) = &open_result {
                if message.contains("Permission denied") {
                    // Some CI/sandbox environments disallow openpty.
                    return;
                }
            }
            open_result.expect("sim open should spawn a terminal session");

            for _ in 0..60 {
                app.poll_connection_events();
                let pty_has_marker = app
                    .pty_sessions
                    .values()
                    .next()
                    .map(|s| s.parser.screen().contents().contains("terminal-ok"))
                    .unwrap_or(false);
                let pipe_has_marker = app
                    .connections
                    .selected_ref()
                    .map(|tab| tab.lines.iter().any(|line| line.contains("terminal-ok")))
                    .unwrap_or(false);
                if pty_has_marker || pipe_has_marker {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            if cfg!(windows) {
                assert!(
                    !app.pty_sessions.is_empty() || !app.pipe_sessions.is_empty(),
                    "expected PTY or pipe session on Windows"
                );
                let found = app
                    .pty_sessions
                    .values()
                    .next()
                    .map(|s| s.parser.screen().contents().contains("terminal-ok"))
                    .unwrap_or(false)
                    || app
                        .connections
                        .selected_ref()
                        .map(|tab| tab.lines.iter().any(|line| line.contains("terminal-ok")))
                        .unwrap_or(false);
                assert!(found, "expected terminal marker in PTY or pipe output");
            } else {
                assert_eq!(app.pty_sessions.len(), 1);
                let parsed = app
                    .pty_sessions
                    .values()
                    .next()
                    .expect("session should exist")
                    .parser
                    .screen()
                    .contents();
                assert!(
                    parsed.contains("terminal-ok"),
                    "expected PTY buffer to contain echoed marker, got: {parsed}"
                );
            }

            let tab_id = app
                .connections
                .selected()
                .expect("selected connection tab should exist");
            app.close_connection_tab(tab_id);
        }

        #[test]
        fn search_terms_from_rules_splits_include_and_exclude() {
            let rules = vec![
                SearchRuleInput {
                    kind: SearchRuleKind::Include,
                    term: "orders".to_string(),
                },
                SearchRuleInput {
                    kind: SearchRuleKind::Exclude,
                    term: "legacy".to_string(),
                },
                SearchRuleInput {
                    kind: SearchRuleKind::Include,
                    term: "  ".to_string(),
                },
            ];

            let (includes, excludes) = search_terms_from_rules(&rules);
            assert_eq!(includes, vec!["orders"]);
            assert_eq!(excludes, vec!["legacy"]);
        }

        #[test]
        fn rules_from_search_terms_roundtrip() {
            let rules = rules_from_search_terms(
                &["orders".to_string(), "platform".to_string()],
                &["legacy".to_string()],
            );
            let (includes, excludes) = search_terms_from_rules(&rules);
            assert_eq!(includes, vec!["orders", "platform"]);
            assert_eq!(excludes, vec!["legacy"]);
        }

        #[test]
        fn rules_from_search_terms_empty_creates_default_rule() {
            let rules = rules_from_search_terms(&[], &[]);
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].kind, SearchRuleKind::Include);
            assert!(rules[0].term.is_empty());
        }

        #[test]
        fn states_from_state_filter_none_is_empty() {
            assert!(states_from_state_filter("").is_empty());
        }

        #[test]
        fn states_from_state_filter_value_is_single_state() {
            assert_eq!(
                states_from_state_filter("running"),
                vec!["running".to_string()]
            );
        }

        #[test]
        fn state_filter_from_saved_states_uses_first_supported_state() {
            assert_eq!(
                state_filter_from_saved_states(&["stopped".to_string(), "running".to_string()]),
                "stopped".to_string()
            );
            assert_eq!(
                state_filter_from_saved_states(&["unknown".to_string()]),
                "".to_string()
            );
        }

        #[test]
        fn log_filters_include_expected_levels() {
            let mut filters = LogFilters::default();
            assert!(filters.includes(LogLevel::Info));
            assert!(!filters.includes(LogLevel::Debug));
            assert!(!filters.includes(LogLevel::Trace));

            filters.set_verbosity_high();
            assert!(filters.includes(LogLevel::Debug));
            assert!(filters.includes(LogLevel::Trace));
        }

        #[test]
        fn app_log_is_capped_to_max_lines() {
            let mut app = Ec2GuiApp::new(GuiOptions {
                mode: Mode::Sim,
                region: None,
                dry_run: true,
            });

            for i in 0..(Ec2GuiApp::MAX_LOG_LINES + 5) {
                app.log_info(format!("line-{i}"));
            }

            assert_eq!(app.logs.len(), Ec2GuiApp::MAX_LOG_LINES);
            assert_eq!(
                app.logs.front().map(|e| e.message.as_str()),
                Some("line-5")
            );
            assert_eq!(
                app.logs.back().map(|e| e.message.as_str()),
                Some("line-20004")
            );
        }

        #[test]
        fn profile_choice_change_detected_when_mtime_differs() {
            let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
            let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
            assert!(profile_choice_changed(Some(t1), Some(t2)));
            assert!(profile_choice_changed(Some(t1), None));
            assert!(profile_choice_changed(None, Some(t2)));
        }

        #[test]
        fn profile_choice_change_not_detected_when_mtime_same() {
            let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
            assert!(!profile_choice_changed(Some(t1), Some(t1)));
            assert!(!profile_choice_changed(None, None));
        }

        #[test]
        fn profile_change_debounce_elapsed_when_duration_met() {
            let started = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(13);
            assert!(profile_change_debounce_elapsed(
                Some(started),
                now,
                Duration::from_secs(2)
            ));
        }

        #[test]
        fn profile_change_debounce_not_elapsed_when_too_soon_or_missing_start() {
            let started = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(11);
            assert!(!profile_change_debounce_elapsed(
                Some(started),
                now,
                Duration::from_secs(2)
            ));
            assert!(!profile_change_debounce_elapsed(
                None,
                now,
                Duration::from_secs(2)
            ));
        }

        #[test]
        fn initial_terminal_id_prefers_configured_terminal() {
            let mut config = AppConfig::default();
            config.default_terminal = Some("kitty".to_string());
            let terminals = vec![
                TerminalOption {
                    id: "xterm".to_string(),
                    display_name: "XTerm".to_string(),
                    kind: ec2_manager::models::TerminalKind::Xterm,
                    program: "xterm".to_string(),
                },
                TerminalOption {
                    id: "kitty".to_string(),
                    display_name: "Kitty".to_string(),
                    kind: ec2_manager::models::TerminalKind::Kitty,
                    program: "kitty".to_string(),
                },
            ];

            assert_eq!(initial_terminal_id(&config, &terminals), "kitty");
        }

        #[test]
        fn initial_terminal_id_empty_when_no_terminals() {
            let config = AppConfig::default();
            assert!(initial_terminal_id(&config, &[]).is_empty());
        }

        #[test]
        fn format_connection_summary_line_contains_instance_ip_and_status() {
            let line =
                format_connection_summary_line("api-a", "i-123", "10.0.1.25", true);
            assert!(line.contains("Instance: api-a (i-123)"));
            assert!(line.contains("Private IP: 10.0.1.25"));
            assert!(line.contains("Status: Running"));
            assert!(line.contains('\t'));
        }

        #[test]
        fn selected_region_label_prefers_selected_region() {
            assert_eq!(
                selected_region_label(Some("eu-central-1"), Some("us-east-1")),
                "eu-central-1".to_string()
            );
        }

        #[test]
        fn selected_region_label_displays_auto_with_context_region() {
            assert_eq!(
                selected_region_label(None, Some("us-west-2")),
                "(auto) (us-west-2)".to_string()
            );
            assert_eq!(selected_region_label(None, None), "(auto)".to_string());
        }

        #[test]
        fn auto_send_prefilled_command_is_disabled_for_dry_run_only() {
            assert!(!should_auto_send_prefilled_command(true));
            assert!(should_auto_send_prefilled_command(false));
        }

        #[test]
        fn parse_bool_env_understands_common_values() {
            assert!(parse_bool_env(Some("true"), false));
            assert!(parse_bool_env(Some("1"), false));
            assert!(parse_bool_env(Some("YES"), false));
            assert!(!parse_bool_env(Some("false"), true));
            assert!(!parse_bool_env(Some("0"), true));
            assert!(!parse_bool_env(Some("off"), true));
            assert!(parse_bool_env(Some("unknown"), true));
            assert!(!parse_bool_env(None, false));
        }

        #[test]
        fn should_try_pty_from_env_defaults_to_true_unless_forced() {
            assert!(should_try_pty_from_env(None));
            assert!(should_try_pty_from_env(Some("0")));
            assert!(!should_try_pty_from_env(Some("1")));
            assert!(!should_try_pty_from_env(Some("true")));
        }

        #[test]
        fn panic_payload_to_string_handles_string_and_static_str() {
            let owned: Box<dyn std::any::Any + Send> = Box::new("owned panic".to_string());
            assert_eq!(panic_payload_to_string(owned.as_ref()), "owned panic");

            let static_str: Box<dyn std::any::Any + Send> = Box::new("static panic");
            assert_eq!(panic_payload_to_string(static_str.as_ref()), "static panic");
        }

        #[test]
        fn panic_log_path_uses_expected_file_name() {
            let path = panic_log_path();
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("ec2_manager_gui_panic.log")
            );
        }

        #[test]
        fn gui_smoke_match_detects_expected_marker() {
            assert!(gui_smoke_match_in_bytes(
                "session open",
                b"[SIM MODE] session open for i-sim0001"
            ));
            assert!(!gui_smoke_match_in_bytes("not-there", b"hello"));
        }

        #[test]
        fn write_gui_smoke_marker_creates_parent_and_writes_payload() {
            let base = std::env::temp_dir().join(format!(
                "ec2-manager-gui-smoke-{}",
                now_unix()
            ));
            let marker_path = base.join("nested").join("marker.txt");
            write_gui_smoke_marker(&marker_path, 7, "session open")
                .expect("marker write should succeed");
            let content = fs::read_to_string(&marker_path).expect("marker should be readable");
            assert!(content.contains("PASS"));
            assert!(content.contains("tab_id=7"));
            assert!(content.contains("expected=session open"));
            let _ = fs::remove_file(&marker_path);
            let _ = fs::remove_dir_all(&base);
        }

        #[test]
        fn terminal_event_payload_maps_ctrl_c_enter_and_paste() {
            let ctrl_c = egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            };
            assert_eq!(terminal_event_payload(&ctrl_c), Some(vec![0x03]));

            let enter = egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            };
            assert_eq!(terminal_event_payload(&enter), Some(b"\r".to_vec()));

            let paste = egui::Event::Paste("echo hi".to_string());
            assert_eq!(
                terminal_event_payload(&paste),
                Some("echo hi".as_bytes().to_vec())
            );
        }

        #[test]
        fn terminal_event_payload_falls_back_for_letter_keys() {
            let key_a = egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            };
            assert_eq!(terminal_event_payload(&key_a), Some(vec![b'a']));

            let key_a_shift = egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            };
            assert_eq!(terminal_event_payload(&key_a_shift), Some(vec![b'A']));
        }

        #[test]
        fn terminal_text_with_cursor_places_cursor_marker() {
            let mut parser = vt100::Parser::new(3, 20, 100);
            parser.process(b"hello");
            let text = terminal_text_with_cursor(parser.screen(), true);
            assert!(text.lines().next().unwrap_or_default().contains("hello|"));
        }

        #[test]
        fn terminal_text_with_cursor_can_hide_marker() {
            let mut parser = vt100::Parser::new(2, 10, 100);
            parser.process(b"abc");
            let text = terminal_text_with_cursor(parser.screen(), false);
            assert!(!text.lines().next().unwrap_or_default().contains('|'));
        }
    }
}

#[cfg(feature = "gui")]
fn main() {
    gui::run();
}
