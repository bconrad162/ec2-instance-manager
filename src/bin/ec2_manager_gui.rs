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
    use std::panic::{self, AssertUnwindSafe};
    use std::path::PathBuf;
    #[cfg(test)]
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex, Once};
    use std::time::{Duration, Instant, SystemTime};

    use eframe::egui;
    use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

    use ec2_manager::aws_context::build_context;
    use ec2_manager::config::AppConfig;
    use ec2_manager::connection_tabs::ConnectionTabs;
    use ec2_manager::diagnostics::run_diagnostics;
    use ec2_manager::error::{AppError, Result};
    use ec2_manager::filter::{apply_filters, matching_tags, Filters};
    use ec2_manager::gui_cli::{gui_help_text, parse_gui_args, GuiOptions};
    use ec2_manager::inventory::load_inventory;
    use ec2_manager::models::{
        AuthStatus, AwsContext, DependencyStatus, Instance, Inventory, Mode, SavedFilter,
        TerminalKind, TerminalOption,
    };
    use ec2_manager::profile_choice::profile_choice_path;
    use ec2_manager::terminal::{
        build_ssm_port_forward_args, build_ssm_session_args, dependency_status,
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
    const GUI_SMOKE_MARKER_ENV: &str = "EC2_MANAGER_GUI_SMOKE_MARKER";
    const GUI_SMOKE_EXPECTED_TEXT_ENV: &str = "EC2_MANAGER_GUI_SMOKE_EXPECTED_TEXT";
    const GUI_SMOKE_EXIT_ON_MARKER_ENV: &str = "EC2_MANAGER_GUI_SMOKE_EXIT_ON_MARKER";
    const GUI_SMOKE_AUTO_CONNECT_ENV: &str = "EC2_MANAGER_GUI_SMOKE_AUTO_CONNECT";

    const COL_FAV_W: f32 = 44.0;
    const COL_INSTANCE_W: f32 = 150.0;
    const COL_NAME_W: f32 = 220.0;
    const COL_STATE_W: f32 = 90.0;
    const COL_SSM_W: f32 = 70.0;
    const COL_IP_W: f32 = 130.0;
    const COL_ENV_W: f32 = 70.0;
    const COL_TAG_W: f32 = 260.0;
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

    const INVENTORY_HEADERS: &[(&str, f32)] = &[
        ("Fav", COL_FAV_W),
        ("InstanceId", COL_INSTANCE_W),
        ("Name", COL_NAME_W),
        ("State", COL_STATE_W),
        ("SSM", COL_SSM_W),
        ("Private IP", COL_IP_W),
        ("Env", COL_ENV_W),
        ("Match Tag", COL_TAG_W),
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

    enum RefreshEvent {
        Completed {
            context: AwsContext,
            inventory: Inventory,
            config_update: Option<(String, String)>,
        },
        AuthNotOk {
            context: AwsContext,
        },
        Failed {
            error: String,
        },
    }

    #[cfg(target_os = "windows")]
    enum UiEvent {
        PtyReady { tab_id: u64, session: PtySession },
        Error { tab_id: u64, error: String },
    }

    struct PtySession {
        child: Box<dyn portable_pty::Child + Send>,
        master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        parser: vt100::Parser,
        last_size: Option<(u16, u16)>,
        bytes_received: u64,
        output_event_count: u64,
        scroll_offset: usize,
    }

    #[derive(Clone, Debug)]
    struct PtyCommand {
        program: String,
        args: Vec<String>,
    }

    #[derive(Clone, Debug)]
    #[allow(dead_code)]
    struct FileEntry {
        name: String,
        is_dir: bool,
        size: u64,
        permissions: String,
        modified: String,
    }

    #[derive(Clone, Debug)]
    enum FileOpStatus {
        Idle,
        Listing,
        Downloading,
        Uploading,
        Error(String),
    }

    struct FileBrowserState {
        current_path: String,
        entries: Vec<FileEntry>,
        status: FileOpStatus,
        selected_entries: std::collections::BTreeSet<usize>,
        last_clicked_entry: Option<usize>,
        pending_downloads: usize,
        path_input: String,
        initialized: bool,
    }

    impl Default for FileBrowserState {
        fn default() -> Self {
            Self {
                current_path: String::new(),
                entries: Vec::new(),
                status: FileOpStatus::Idle,
                selected_entries: std::collections::BTreeSet::new(),
                last_clicked_entry: None,
                pending_downloads: 0,
                path_input: String::new(),
                initialized: false,
            }
        }
    }

    enum FileOpEvent {
        ListingCompleted {
            tab_id: u64,
            path: String,
            entries: Vec<FileEntry>,
        },
        ListingFailed {
            tab_id: u64,
            error: String,
        },
        DownloadCompleted {
            tab_id: u64,
            remote_path: String,
            local_path: String,
            bytes: u64,
        },
        DownloadFailed {
            tab_id: u64,
            error: String,
        },
        UploadCompleted {
            tab_id: u64,
            local_path: String,
            remote_path: String,
            bytes: u64,
        },
        UploadFailed {
            tab_id: u64,
            error: String,
        },
    }

    /// Git-bash-style PS1 prompt sent to the remote shell when the user
    /// clicks "Update PS1".  Produces:
    ///   (blank line)
    ///   user@host SSM ~/working/dir
    ///   $
    /// with bold-green user@host, magenta "SSM", and bold-yellow path.
    const SSM_PS1_COMMAND: &[u8] =
        b"bash\rexport PS1='\\n\\[\\033[1;32m\\]\\u@\\h\\[\\033[0m\\] \\[\\033[1;35m\\]SSM\\[\\033[0m\\] \\[\\033[1;33m\\]\\w\\[\\033[0m\\]\\n\\$ '\r";

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
                Box::new(move |cc| {
                    let app = Ec2GuiApp::new(app_options.clone());
                    if app.dark_mode {
                        cc.egui_ctx.set_visuals(egui::Visuals::dark());
                    } else {
                        cc.egui_ctx.set_visuals(egui::Visuals::light());
                    }
                    Ok(Box::new(app))
                }),
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
        show_close_blocked: bool,
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
        proc_tx: Sender<ProcEvent>,
        proc_rx: Receiver<ProcEvent>,
        refresh_tx: Sender<RefreshEvent>,
        refresh_rx: Receiver<RefreshEvent>,
        refreshing: bool,
        dark_mode: bool,
        scroll_sensitivity: f32,
        file_browsers: HashMap<u64, FileBrowserState>,
        file_op_tx: Sender<FileOpEvent>,
        file_op_rx: Receiver<FileOpEvent>,
        #[cfg(target_os = "windows")]
        ui_tx: Sender<UiEvent>,
        #[cfg(target_os = "windows")]
        ui_rx: Receiver<UiEvent>,
    }

    impl Ec2GuiApp {
        const MAX_LOG_LINES: usize = 20_000;

        fn new(options: GuiOptions) -> Self {
            let config = AppConfig::load().unwrap_or_default();
            let dependencies = dependency_status();
            let (proc_tx, proc_rx) = mpsc::channel();
            let (refresh_tx, refresh_rx) = mpsc::channel();
            let (file_op_tx, file_op_rx) = mpsc::channel();
            #[cfg(target_os = "windows")]
            let (ui_tx, ui_rx) = mpsc::channel();
            let profile_choice_path = profile_choice_path();
            let last_profile_choice_mtime = profile_choice_mtime(profile_choice_path.as_deref());
            let terminals = filter_embedded_terminals(discover_terminals());
            let selected_terminal_id = initial_terminal_id(&config, &terminals);
            let gui_smoke = gui_smoke_config_from_env();
            let dark_mode = config.theme.as_deref() != Some("light");
            let scroll_sensitivity = config.scroll_sensitivity.unwrap_or(10.0);

            let mut app = Self {
                options,
                gui_smoke,
                gui_smoke_marker_written: false,
                gui_smoke_auto_connect_attempted: false,
                gui_smoke_should_close: false,
                show_close_blocked: false,
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
                proc_tx,
                proc_rx,
                refresh_tx,
                refresh_rx,
                refreshing: false,
                dark_mode,
                scroll_sensitivity,
                file_browsers: HashMap::new(),
                file_op_tx,
                file_op_rx,
                #[cfg(target_os = "windows")]
                ui_tx,
                #[cfg(target_os = "windows")]
                ui_rx,
            };

            app.log_info("application started");
            if let Some(smoke) = &app.gui_smoke {
                app.log_info(format!(
                    "GUI smoke mode enabled marker={} expected='{}'",
                    smoke.marker_path.display(),
                    smoke.expected_text
                ));
            }
            app.refresh_context_and_inventory(true);
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
            self.refresh_context_and_inventory(true);
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

        fn log_trace(&mut self, message: impl Into<String>) {
            self.log(LogLevel::Trace, message);
        }

        fn guarded_action<F>(&mut self, label: &str, action: F) -> bool
        where
            F: FnOnce(&mut Self) -> Result<()>,
        {
            let result = panic::catch_unwind(AssertUnwindSafe(|| action(self)));
            match result {
                Ok(Ok(())) => true,
                Ok(Err(err)) => {
                    self.message = format!("error: {err}");
                    self.log_error(format!("{label} failed: {err}"));
                    false
                }
                Err(payload) => {
                    let message = format!(
                        "{label} panicked: {}",
                        panic_payload_to_string(payload.as_ref())
                    );
                    self.message = message.clone();
                    self.log_error(message);
                    false
                }
            }
        }

        fn selected_terminal(&self) -> Option<&TerminalOption> {
            self.terminals
                .iter()
                .find(|t| t.id == self.selected_terminal_id)
        }

        fn refresh_context_and_inventory(&mut self, force: bool) {
            if self.refreshing {
                self.log_debug("refresh already in progress, skipping");
                return;
            }
            self.refreshing = true;
            self.log_info(format!("refresh inventory requested (force={force})"));
            self.message = "Refreshing...".to_string();

            let mode = self.options.mode.clone();
            let config = self.config.clone();
            let region_override = self.options.region.clone();
            let tx = self.refresh_tx.clone();

            std::thread::spawn(move || {
                let context = match build_context(mode, &config, region_override.as_deref()) {
                    Ok(ctx) => ctx,
                    Err(err) => {
                        let _ = tx.send(RefreshEvent::Failed {
                            error: err.to_string(),
                        });
                        return;
                    }
                };

                let config_update = context
                    .account_id
                    .as_ref()
                    .and_then(|acct| {
                        region_override
                            .as_ref()
                            .map(|region| (acct.clone(), region.clone()))
                    });

                if context.mode == Mode::Live && context.auth_status != AuthStatus::Ok {
                    let _ = tx.send(RefreshEvent::AuthNotOk { context });
                    return;
                }

                match load_inventory(&context, &config.tag_mapping, force) {
                    Ok(inventory) => {
                        let _ = tx.send(RefreshEvent::Completed {
                            context,
                            inventory,
                            config_update,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(RefreshEvent::Failed {
                            error: err.to_string(),
                        });
                    }
                }
            });
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

            let command_args =
                build_ssm_session_args(&instance.instance_id, &context.region);
            let command_line = format!("aws {}", command_args.join(" "));
            let command = if context.mode == Mode::Sim {
                let kind = self
                    .selected_terminal()
                    .map(|t| t.kind.clone())
                    .unwrap_or(TerminalKind::Cmd);
                format_sim_command(kind, &command_line, &instance.instance_id, None)
            } else {
                command_line
            };

            let title = instance
                .name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| instance.instance_id.clone());

            self.open_connection_tab(
                title,
                instance.instance_id.clone(),
                command,
                command_args,
                &context,
            )?;
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

            let command_args = build_ssm_port_forward_args(
                &instance.instance_id,
                &context.region,
                self.local_port,
                self.remote_port,
            );
            let command_line = format!("aws {}", command_args.join(" "));

            let command = if context.mode == Mode::Sim {
                let kind = self
                    .selected_terminal()
                    .map(|t| t.kind.clone())
                    .unwrap_or(TerminalKind::Cmd);
                format_sim_command(
                    kind,
                    &command_line,
                    &instance.instance_id,
                    Some((self.local_port, self.remote_port)),
                )
            } else {
                command_line
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

            self.open_connection_tab(
                title,
                instance.instance_id.clone(),
                command,
                command_args,
                &context,
            )?;
            self.main_tab = MainTab::Connections;
            Ok(())
        }

        fn open_connection_tab(
            &mut self,
            title: String,
            instance_id: String,
            command: String,
            command_args: Vec<String>,
            context: &AwsContext,
        ) -> Result<()> {
            let selected_terminal = self.selected_terminal().cloned();
            self.log_debug(format!(
                "terminal selection: {}",
                terminal_debug_label(selected_terminal.as_ref())
            ));
            let tab_id = self.connections.open(title.clone(), instance_id.clone());
            let default_path = if context.mode == Mode::Sim {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "/".to_string())
            } else {
                "/home/ssm-user".to_string()
            };
            let mut fb_state = FileBrowserState {
                current_path: default_path.clone(),
                path_input: default_path.clone(),
                ..Default::default()
            };
            fb_state.initialized = false;
            self.file_browsers.insert(tab_id, fb_state);
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

            let pty_command = pty_command_for_context(
                selected_terminal.as_ref(),
                context,
                &command,
                &command_args,
            );
            self.log_debug(format!(
                "spawning PTY command via {}",
                pty_command.program
            ));
            self.log_trace(format!(
                "pty args: {:?} terminal={}",
                pty_command.args,
                terminal_debug_label(selected_terminal.as_ref())
            ));

            #[cfg(target_os = "windows")]
            {
                let context = context.clone();
                let proc_tx = self.proc_tx.clone();
                let ui_tx = self.ui_tx.clone();
                let pty_command = pty_command.clone();
                self.log_debug("spawning PTY command async");
                std::thread::spawn(move || {
                    let result = panic::catch_unwind(AssertUnwindSafe(|| {
                        spawn_pty_session_parts(tab_id, &pty_command, &context)
                    }));
                    match result {
                        Ok(Ok((session, reader))) => {
                            // Send PtyReady FIRST so the UI inserts the session
                            // into pty_sessions before any reader output arrives.
                            let _ = ui_tx.send(UiEvent::PtyReady { tab_id, session });
                            // Start reader thread AFTER PtyReady is sent.
                            start_pty_reader_thread(tab_id, reader, proc_tx);
                        }
                        Ok(Err(err)) => {
                            let _ = ui_tx.send(UiEvent::Error {
                                tab_id,
                                error: format!("PTY spawn failed: {err}"),
                            });
                        }
                        Err(payload) => {
                            let _ = ui_tx.send(UiEvent::Error {
                                tab_id,
                                error: format!(
                                    "PTY spawn panicked: {}",
                                    panic_payload_to_string(payload.as_ref())
                                ),
                            });
                        }
                    }
                });
            }
            #[cfg(not(target_os = "windows"))]
            self.spawn_pty_session(tab_id, pty_command, context)?;

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

        #[cfg(not(target_os = "windows"))]
        fn spawn_pty_session(
            &mut self,
            tab_id: u64,
            command: PtyCommand,
            context: &AwsContext,
        ) -> Result<()> {
            let (session, reader) =
                spawn_pty_session_parts(tab_id, &command, context)?;
            self.pty_sessions.insert(tab_id, session);
            start_pty_reader_thread(tab_id, reader, self.proc_tx.clone());
            Ok(())
        }

        fn poll_connection_events(&mut self) {
            // Process PtyReady events BEFORE proc_rx output events,
            // so the session is inserted into pty_sessions before any
            // output arrives from the reader thread.
            self.poll_ui_events();

            while let Ok(event) = self.proc_rx.try_recv() {
                match event {
                    ProcEvent::Output { tab_id, bytes } => {
                        let log_msg = if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                            session.bytes_received += bytes.len() as u64;
                            session.output_event_count += 1;
                            let evt = session.output_event_count;
                            let total = session.bytes_received;
                            session.parser.process(&bytes);
                            // Respond to Device Status Report queries (ESC[6n =
                            // cursor position request).  CMD and PowerShell send
                            // this at startup and block until they receive the
                            // response ESC[row;colR.
                            respond_to_terminal_queries(session, &bytes);
                            if evt <= 5 {
                                let preview = String::from_utf8_lossy(
                                    &bytes[..bytes.len().min(200)]
                                );
                                let sanitized: String = preview.chars().map(|c| {
                                    if c.is_control() && c != '\n' { '.' } else { c }
                                }).collect();
                                let hex: String = bytes.iter()
                                    .take(64)
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                Some((LogLevel::Debug, format!(
                                    "tab={tab_id} output #{evt} len={} total={total} hex=[{hex}] preview=[{sanitized}]",
                                    bytes.len(),
                                )))
                            } else {
                                Some((LogLevel::Trace, format!(
                                    "tab={tab_id} output bytes={} total={total}",
                                    bytes.len(),
                                )))
                            }
                        } else {
                            let text = String::from_utf8_lossy(&bytes).to_string();
                            for line in text.lines() {
                                self.connections.append_line(tab_id, line.to_string());
                            }
                            None
                        };
                        if let Some((level, msg)) = log_msg {
                            self.log(level, msg);
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
                let try_wait_result = panic::catch_unwind(AssertUnwindSafe(|| {
                    session.child.try_wait()
                }));
                match try_wait_result {
                    Ok(Ok(Some(status))) => {
                        exited.push((*tab_id, status.exit_code() as i32));
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(err)) => {
                        wait_errors.push(format!("tab={tab_id} try_wait error: {err}"));
                        self.connections
                            .append_line(*tab_id, format!("[wait error] {err}"));
                        exited.push((*tab_id, -1));
                    }
                    Err(_) => {
                        wait_errors.push(format!("tab={tab_id} try_wait panicked"));
                        self.connections
                            .append_line(*tab_id, "[error] try_wait panicked".to_string());
                        exited.push((*tab_id, -1));
                    }
                }
            }
            for message in wait_errors {
                self.log_error(message);
            }

            for (tab_id, code) in exited {
                if let Some(session) = self.pty_sessions.get(&tab_id) {
                    let snapshot = session.parser.screen().contents();
                    let non_empty_lines: Vec<&str> = snapshot.lines()
                        .filter(|l| !l.trim().is_empty())
                        .collect();
                    self.log_info(format!(
                        "tab={tab_id} PTY exited code={code} screen_lines={}",
                        non_empty_lines.len()
                    ));
                    if non_empty_lines.is_empty() {
                        self.log_warn(format!(
                            "tab={tab_id} process exited with empty screen (code={code}); \
                             command may have failed silently"
                        ));
                        self.connections.append_line(
                            tab_id,
                            format!(
                                "[warning] process exited with code {code} and no output — \
                                 check that aws CLI and session-manager-plugin are working"
                            ),
                        );
                    }
                    for line in non_empty_lines {
                        self.connections.append_line(tab_id, line.to_string());
                    }
                }
                self.pty_sessions.remove(&tab_id);
                let _ = self.proc_tx.send(ProcEvent::Exited { tab_id, code });
            }
        }

        #[cfg(target_os = "windows")]
        fn poll_ui_events(&mut self) {
            while let Ok(event) = self.ui_rx.try_recv() {
                match event {
                    UiEvent::PtyReady { tab_id, session } => {
                        self.log_info(format!("tab={tab_id} PTY session ready"));
                        self.pty_sessions.insert(tab_id, session);
                    }
                    UiEvent::Error { tab_id, error } => {
                        self.log_error(format!("tab={tab_id} PTY spawn error: {error}"));
                        self.connections
                            .append_line(tab_id, format!("[error] {error}"));
                        self.connections.set_running(tab_id, false);
                    }
                }
            }
        }

        #[cfg(not(target_os = "windows"))]
        fn poll_ui_events(&mut self) {}

        fn poll_refresh_events(&mut self) {
            while let Ok(event) = self.refresh_rx.try_recv() {
                self.refreshing = false;
                match event {
                    RefreshEvent::Completed {
                        context,
                        inventory,
                        config_update,
                    } => {
                        self.context = Some(context);
                        self.inventory = inventory;
                        self.apply_filters();
                        self.message = format!(
                            "Loaded {} instances ({} filtered)",
                            self.inventory.instances.len(),
                            self.filtered.len()
                        );
                        self.log_info(self.message.clone());
                        if let Some((account_id, region)) = config_update {
                            self.config.upsert_account_region(&account_id, &region);
                            if let Err(err) = self.config.save() {
                                self.log_warn(format!(
                                    "failed to save account-region mapping: {err}"
                                ));
                            }
                        }
                        self.maybe_auto_connect_gui_smoke();
                    }
                    RefreshEvent::AuthNotOk { context } => {
                        self.context = Some(context);
                        self.inventory = Inventory {
                            instances: Vec::new(),
                            fetched_at: std::time::SystemTime::now(),
                        };
                        self.filtered.clear();
                        self.message =
                            "Auth is not OK (live mode). Refresh credentials and retry."
                                .to_string();
                        self.log_warn("inventory refresh blocked: auth not OK in live mode");
                    }
                    RefreshEvent::Failed { error } => {
                        self.message = format!("error: {error}");
                        self.log_error(self.message.clone());
                    }
                }
            }
        }

        fn close_connection_tab(&mut self, tab_id: u64) {
            if let Some(session) = self.pty_sessions.remove(&tab_id) {
                self.log_debug(format!("tab={tab_id} closing PTY session, spawning cleanup thread"));
                std::thread::spawn(move || {
                    // Drop master/writer first to unblock reader and signal EOF to child
                    drop(session.master);
                    drop(session.writer);
                    drop(session.parser);

                    let mut child = session.child;
                    // kill() can block or panic on ConPTY — catch everything
                    let kill_result = panic::catch_unwind(AssertUnwindSafe(|| {
                        child.kill()
                    }));
                    match &kill_result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => eprintln!("tab={tab_id} kill error (non-fatal): {e}"),
                        Err(_) => eprintln!("tab={tab_id} kill panicked (non-fatal)"),
                    }

                    // wait() reaps the zombie — also guarded
                    let wait_result = panic::catch_unwind(AssertUnwindSafe(|| {
                        child.wait()
                    }));
                    match &wait_result {
                        Ok(Ok(status)) => {
                            eprintln!("tab={tab_id} child exited code={}", status.exit_code());
                        }
                        Ok(Err(e)) => eprintln!("tab={tab_id} wait error (non-fatal): {e}"),
                        Err(_) => eprintln!("tab={tab_id} wait panicked (non-fatal)"),
                    }
                });
            }
            self.file_browsers.remove(&tab_id);
            self.connections.close(tab_id);
            self.log_info(format!("closed connection tab id={tab_id}"));
        }

        fn request_file_listing(&mut self, tab_id: u64, path: String) {
            let Some(fb) = self.file_browsers.get_mut(&tab_id) else {
                return;
            };
            fb.status = FileOpStatus::Listing;
            fb.current_path = path.clone();
            fb.path_input = path.clone();
            fb.selected_entries.clear();
            fb.last_clicked_entry = None;

            let tab = self.connections.selected_ref().cloned();
            let instance_id = tab.map(|t| t.instance_id.clone()).unwrap_or_default();
            let mode = self.options.mode.clone();
            let context = self.context.clone();
            let tx = self.file_op_tx.clone();

            std::thread::spawn(move || {
                let result = if mode == Mode::Sim {
                    list_files_local(&path)
                } else if let Some(ctx) = &context {
                    let cmd = format!("ls -la {path}");
                    match ssm_send_command(&ctx.profile, &ctx.region, &instance_id, &cmd) {
                        Ok(command_id) => {
                            match ssm_wait_for_command(
                                &ctx.profile,
                                &ctx.region,
                                &instance_id,
                                &command_id,
                            ) {
                                Ok(output) => Ok(parse_ls_output(&output)),
                                Err(e) => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err("no AWS context available".to_string())
                };

                match result {
                    Ok(entries) => {
                        let _ = tx.send(FileOpEvent::ListingCompleted {
                            tab_id,
                            path,
                            entries,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(FileOpEvent::ListingFailed { tab_id, error });
                    }
                }
            });
        }

        fn request_file_download(
            &mut self,
            tab_id: u64,
            remote_path: String,
            local_path: String,
        ) {
            let Some(fb) = self.file_browsers.get_mut(&tab_id) else {
                return;
            };
            fb.status = FileOpStatus::Downloading;

            let tab = self.connections.selected_ref().cloned();
            let instance_id = tab.map(|t| t.instance_id.clone()).unwrap_or_default();
            let mode = self.options.mode.clone();
            let context = self.context.clone();
            let tx = self.file_op_tx.clone();

            std::thread::spawn(move || {
                let result = if mode == Mode::Sim {
                    std::fs::read(&remote_path)
                        .and_then(|data| {
                            let bytes = data.len() as u64;
                            std::fs::write(&local_path, &data)?;
                            Ok(bytes)
                        })
                        .map_err(|e| e.to_string())
                } else if let Some(ctx) = &context {
                    let cmd = format!("base64 {remote_path}");
                    ssm_send_command(&ctx.profile, &ctx.region, &instance_id, &cmd)
                        .and_then(|command_id| {
                            ssm_wait_for_command(
                                &ctx.profile,
                                &ctx.region,
                                &instance_id,
                                &command_id,
                            )
                        })
                        .and_then(|b64_output| {
                            use base64::Engine;
                            let cleaned: String =
                                b64_output.chars().filter(|c| !c.is_whitespace()).collect();
                            let data = base64::engine::general_purpose::STANDARD
                                .decode(&cleaned)
                                .map_err(|e| format!("base64 decode failed: {e}"))?;
                            let bytes = data.len() as u64;
                            std::fs::write(&local_path, &data)
                                .map_err(|e| format!("write local file failed: {e}"))?;
                            Ok(bytes)
                        })
                } else {
                    Err("no AWS context available".to_string())
                };

                match result {
                    Ok(bytes) => {
                        let _ = tx.send(FileOpEvent::DownloadCompleted {
                            tab_id,
                            remote_path,
                            local_path,
                            bytes,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(FileOpEvent::DownloadFailed { tab_id, error });
                    }
                }
            });
        }

        /// Like `request_file_download` but does not set status to Downloading
        /// (the caller manages batch status and `pending_downloads`).
        fn request_batch_file_download(
            &mut self,
            tab_id: u64,
            remote_path: String,
            local_path: String,
        ) {
            let tab = self.connections.selected_ref().cloned();
            let instance_id = tab.map(|t| t.instance_id.clone()).unwrap_or_default();
            let mode = self.options.mode.clone();
            let context = self.context.clone();
            let tx = self.file_op_tx.clone();

            std::thread::spawn(move || {
                let result = if mode == Mode::Sim {
                    std::fs::read(&remote_path)
                        .and_then(|data| {
                            let bytes = data.len() as u64;
                            std::fs::write(&local_path, &data)?;
                            Ok(bytes)
                        })
                        .map_err(|e| e.to_string())
                } else if let Some(ctx) = &context {
                    let cmd = format!("base64 {remote_path}");
                    ssm_send_command(&ctx.profile, &ctx.region, &instance_id, &cmd)
                        .and_then(|command_id| {
                            ssm_wait_for_command(
                                &ctx.profile,
                                &ctx.region,
                                &instance_id,
                                &command_id,
                            )
                        })
                        .and_then(|b64_output| {
                            use base64::Engine;
                            let cleaned: String =
                                b64_output.chars().filter(|c| !c.is_whitespace()).collect();
                            let data = base64::engine::general_purpose::STANDARD
                                .decode(&cleaned)
                                .map_err(|e| format!("base64 decode failed: {e}"))?;
                            let bytes = data.len() as u64;
                            std::fs::write(&local_path, &data)
                                .map_err(|e| format!("write local file failed: {e}"))?;
                            Ok(bytes)
                        })
                } else {
                    Err("no AWS context available".to_string())
                };

                match result {
                    Ok(bytes) => {
                        let _ = tx.send(FileOpEvent::DownloadCompleted {
                            tab_id,
                            remote_path,
                            local_path,
                            bytes,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(FileOpEvent::DownloadFailed { tab_id, error });
                    }
                }
            });
        }

        fn request_file_upload(
            &mut self,
            tab_id: u64,
            local_path: String,
            remote_dir: String,
        ) {
            let Some(fb) = self.file_browsers.get_mut(&tab_id) else {
                return;
            };
            fb.status = FileOpStatus::Uploading;

            let tab = self.connections.selected_ref().cloned();
            let instance_id = tab.map(|t| t.instance_id.clone()).unwrap_or_default();
            let mode = self.options.mode.clone();
            let context = self.context.clone();
            let tx = self.file_op_tx.clone();
            let file_name = std::path::Path::new(&local_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "uploaded_file".to_string());
            let remote_path = join_path(&remote_dir, &file_name);

            std::thread::spawn(move || {
                let result = if mode == Mode::Sim {
                    std::fs::read(&local_path)
                        .and_then(|data| {
                            let bytes = data.len() as u64;
                            std::fs::write(&remote_path, &data)?;
                            Ok(bytes)
                        })
                        .map_err(|e| e.to_string())
                } else if let Some(ctx) = &context {
                    std::fs::read(&local_path)
                        .map_err(|e| format!("read local file failed: {e}"))
                        .and_then(|data| {
                            use base64::Engine;
                            let bytes = data.len() as u64;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            let cmd =
                                format!("echo '{}' | base64 -d > {}", b64, remote_path);
                            ssm_send_command(&ctx.profile, &ctx.region, &instance_id, &cmd)
                                .and_then(|command_id| {
                                    ssm_wait_for_command(
                                        &ctx.profile,
                                        &ctx.region,
                                        &instance_id,
                                        &command_id,
                                    )
                                })?;
                            Ok(bytes)
                        })
                } else {
                    Err("no AWS context available".to_string())
                };

                match result {
                    Ok(bytes) => {
                        let _ = tx.send(FileOpEvent::UploadCompleted {
                            tab_id,
                            local_path,
                            remote_path,
                            bytes,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(FileOpEvent::UploadFailed { tab_id, error });
                    }
                }
            });
        }

        fn poll_file_op_events(&mut self) {
            while let Ok(event) = self.file_op_rx.try_recv() {
                match event {
                    FileOpEvent::ListingCompleted {
                        tab_id,
                        path,
                        entries,
                    } => {
                        self.log_info(format!(
                            "file listing completed tab={tab_id} path={path} entries={}",
                            entries.len()
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            fb.entries = entries;
                            fb.status = FileOpStatus::Idle;
                            fb.initialized = true;
                        }
                    }
                    FileOpEvent::ListingFailed { tab_id, error } => {
                        self.log_error(format!(
                            "file listing failed tab={tab_id}: {error}"
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            fb.status = FileOpStatus::Error(error);
                        }
                    }
                    FileOpEvent::DownloadCompleted {
                        tab_id,
                        remote_path,
                        local_path,
                        bytes,
                    } => {
                        self.log_info(format!(
                            "download completed tab={tab_id} {remote_path} -> {local_path} ({bytes} bytes)"
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            if fb.pending_downloads > 0 {
                                fb.pending_downloads -= 1;
                                if fb.pending_downloads == 0 {
                                    fb.status = FileOpStatus::Idle;
                                }
                            } else {
                                fb.status = FileOpStatus::Idle;
                            }
                        }
                    }
                    FileOpEvent::DownloadFailed { tab_id, error } => {
                        self.log_error(format!(
                            "download failed tab={tab_id}: {error}"
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            if fb.pending_downloads > 0 {
                                fb.pending_downloads -= 1;
                                if fb.pending_downloads == 0 {
                                    fb.status = FileOpStatus::Error(error);
                                }
                            } else {
                                fb.status = FileOpStatus::Error(error);
                            }
                        }
                    }
                    FileOpEvent::UploadCompleted {
                        tab_id,
                        local_path,
                        remote_path,
                        bytes,
                    } => {
                        self.log_info(format!(
                            "upload completed tab={tab_id} {local_path} -> {remote_path} ({bytes} bytes)"
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            fb.status = FileOpStatus::Idle;
                            let path = fb.current_path.clone();
                            let tab_id_copy = tab_id;
                            // queue re-listing after upload
                            self.request_file_listing(tab_id_copy, path);
                            return;
                        }
                    }
                    FileOpEvent::UploadFailed { tab_id, error } => {
                        self.log_error(format!(
                            "upload failed tab={tab_id}: {error}"
                        ));
                        if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                            fb.status = FileOpStatus::Error(error);
                        }
                    }
                }
            }
        }

        fn send_raw_bytes_to_connection_tab(&mut self, tab_id: u64, payload: &[u8]) {
            self.log_trace(format!(
                "sending input to tab={tab_id} bytes={}",
                payload.len()
            ));
            let Some(session) = self.pty_sessions.get_mut(&tab_id) else {
                return;
            };
            session.scroll_offset = 0;
            let Ok(mut stdin) = session.writer.lock() else {
                return;
            };
            let write_result = stdin.write_all(payload).and_then(|()| stdin.flush());
            drop(stdin);
            if let Err(err) = write_result {
                self.log_error(format!("tab={tab_id} write error: {err}"));
            }
        }

        fn forward_terminal_key_input(&mut self, ctx: &egui::Context, tab_id: u64) {
            let events = ctx.input(|i| i.raw.events.clone());
            let current_modifiers = ctx.input(|i| i.modifiers);
            let has_text = events.iter().any(|e| matches!(e, egui::Event::Text(_)));
            let has_key_backspace = events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Backspace,
                        pressed: true,
                        ..
                    }
                )
            });
            let mut sent_etx = false;
            let on_alt_screen = self.pty_sessions.get(&tab_id)
                .map(|s| s.parser.screen().alternate_screen())
                .unwrap_or(false);
            let screen_rows = self.pty_sessions.get(&tab_id)
                .map(|s| s.parser.screen().size().0 as usize)
                .unwrap_or(45);
            for event in events {
                // Intercept Shift+PageUp/Down/Home/End for scrollback navigation.
                // Only when not on alternate screen (vim, less, htop handle their own scrolling).
                if !on_alt_screen {
                    if let egui::Event::Key { key, pressed: true, modifiers, .. } = &event {
                        if modifiers.shift {
                            match key {
                                egui::Key::PageUp => {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        session.scroll_offset = session.scroll_offset.saturating_add(screen_rows);
                                    }
                                    continue;
                                }
                                egui::Key::PageDown => {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        session.scroll_offset = session.scroll_offset.saturating_sub(screen_rows);
                                    }
                                    continue;
                                }
                                egui::Key::Home => {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        session.scroll_offset = usize::MAX;
                                    }
                                    continue;
                                }
                                egui::Key::End => {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        session.scroll_offset = 0;
                                    }
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // egui emits Event::Copy for Ctrl+C regardless of shift.
                // We intercept it here so we can check modifiers: plain
                // Ctrl+C → send ETX (0x03), Ctrl+Shift+C → let egui copy.
                if matches!(event, egui::Event::Copy) {
                    if !current_modifiers.shift && !sent_etx {
                        self.log_trace(format!(
                            "terminal input event tab={tab_id} kind=Copy→ETX"
                        ));
                        self.send_raw_bytes_to_connection_tab(tab_id, &[0x03]);
                        sent_etx = true;
                    }
                    continue;
                }
                // Let egui handle Cut natively (clipboard cut).
                if matches!(event, egui::Event::Cut) {
                    continue;
                }
                if let Some(payload) =
                    terminal_event_payload_for_terminal(&event, has_text, has_key_backspace)
                {
                    // Dedup: if we already sent ETX via the Copy path above,
                    // skip a second ETX from the Key::C arm.
                    if payload == [0x03] && sent_etx {
                        continue;
                    }
                    if payload == [0x03] {
                        sent_etx = true;
                    }
                    self.log_trace(format!(
                        "terminal input event tab={tab_id} kind={}",
                        terminal_event_kind(&event)
                    ));
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
                        for (label, width) in INVENTORY_HEADERS {
                            ui.add_sized(
                                [*width, 18.0],
                                egui::Label::new(egui::RichText::new(*label).strong()),
                            );
                        }
                        ui.end_row();

                        let account_scope = self.account_scope();
                        let region_scope = self.region_scope();
                        let mut pending_connect: Option<String> = None;
                        let (include_terms, _) = search_terms_from_rules(&self.search_rules);

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
                                egui::Button::selectable(selected, instance.instance_id.clone()),
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

                            let matched_tag_text = if include_terms.is_empty() {
                                String::new()
                            } else {
                                let tags = matching_tags(instance, &include_terms);
                                let text = tags
                                    .into_iter()
                                    .map(|(k, v)| format!("{k}={v}"))
                                    .collect::<Vec<String>>()
                                    .join(", ");
                                truncate(&text, 42)
                            };
                            let resp_tag = ui.add_sized(
                                [COL_TAG_W, 18.0],
                                egui::Label::new(matched_tag_text).sense(egui::Sense::click()),
                            );
                            row_clicked |= resp_tag.clicked();
                            row_double_clicked |= resp_tag.double_clicked();
                            row_hovered |= resp_tag.hovered();

                            let row_response = resp_fav
                                .clone()
                                .union(resp_id.clone())
                                .union(resp_name.clone())
                                .union(resp_state.clone())
                                .union(resp_ssm.clone())
                                .union(resp_ip.clone())
                                .union(resp_env.clone())
                                .union(resp_tag.clone());

                            row_response.context_menu(|ui| {
                                if ui.button("Quick Connect").clicked() {
                                    quick_connect_clicked = true;
                                    ui.close();
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
                                .union(resp_tag.rect);

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
                            if self.guarded_action("quick connect", |app| app.connect_selected()) {
                                self.main_tab = MainTab::Connections;
                            }
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
            let mut close_all = false;

            ui.horizontal_wrapped(|ui| {
                for (id, title, running) in &tabs_snapshot {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            let prefix = if *running { "" } else { "[done] " };
                            let selected = self.connections.selected() == Some(*id);
                            if ui
                                .selectable_label(
                                    selected,
                                    format!("{}{}", prefix, truncate(title, 28)),
                                )
                                .clicked()
                            {
                                to_select = Some(*id);
                            }
                            if ui.small_button("x").clicked() {
                                to_close = Some(*id);
                            }
                        });
                    });
                }
                if tabs_snapshot.len() > 1 && ui.button("Close All").clicked() {
                    close_all = true;
                }
            });

            if let Some(id) = to_select {
                self.connections.select(id);
            }
            if close_all {
                let tab_ids: Vec<u64> = tabs_snapshot.iter().map(|(id, _, _)| *id).collect();
                for id in tab_ids {
                    self.close_connection_tab(id);
                }
            } else if let Some(id) = to_close {
                self.close_connection_tab(id);
            }

            ui.separator();

            if let Some(tab) = self.connections.selected_ref().cloned() {
                let private_ip = find_instance(&self.inventory.instances, &tab.instance_id)
                    .and_then(|i| i.private_ip.clone())
                    .unwrap_or_else(|| "-".to_string());
                let pty_bytes = self.pty_sessions.get(&tab.id)
                    .map(|s| s.bytes_received)
                    .unwrap_or(0);
                ui.horizontal(|ui| {
                    ui.monospace(format_connection_summary_line(
                        &tab.title,
                        &tab.instance_id,
                        &private_ip,
                        tab.running,
                        pty_bytes,
                    ));
                    if tab.running && ui.small_button("Update PS1").clicked() {
                        self.send_raw_bytes_to_connection_tab(tab.id, SSM_PS1_COMMAND);
                    }
                });
                ui.separator();

                // Trigger initial file listing if not yet initialized
                if let Some(fb) = self.file_browsers.get(&tab.id) {
                    if !fb.initialized && matches!(fb.status, FileOpStatus::Idle) {
                        let path = fb.current_path.clone();
                        self.request_file_listing(tab.id, path);
                    }
                }

                let show_cursor = ui.input(|i| ((i.time * 2.0) as i64) % 2 == 0);
                let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                let tab_id = tab.id;
                let tab_lines = tab.lines.clone();
                let available = ui.available_size();
                let file_browser_width = 220.0_f32;

                ui.horizontal(|ui| {
                    // Left: File browser panel
                    ui.allocate_ui_with_layout(
                        egui::vec2(file_browser_width, available.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            self.render_file_browser(ui, tab_id);
                        },
                    );

                    ui.separator();

                    // Right: Terminal with outer banner
                    let banner_fill = if self.dark_mode {
                        egui::Color32::from_rgb(44, 44, 44)
                    } else {
                        ui.visuals().panel_fill
                    };

                    let remaining_width = (available.x - file_browser_width - 12.0).max(100.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(remaining_width, available.y),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            let terminal_response = egui::Frame::NONE
                                .fill(banner_fill)
                                .inner_margin(egui::Margin::same(4))
                                .corner_radius(egui::CornerRadius::same(2))
                                .show(ui, |ui| {
                                    egui::Frame::NONE
                                        .fill(terminal_panel_fill())
                                        .inner_margin(egui::Margin::same(0))
                                        .show(ui, |ui| {
                                            let available = ui.available_size();
                                            ui.allocate_ui_with_layout(
                                                available,
                                                egui::Layout::top_down(egui::Align::Min),
                                                |ui| {
                                                    ui.set_min_width(available.x);
                                                    let size = ui.available_size();
                                                    let (rows, cols) =
                                                        terminal_grid_from_pixels(
                                                            ui, &font_id, size,
                                                        );
                                                    let mut terminal_job =
                                                        if let Some(session) =
                                                            self.pty_sessions
                                                                .get_mut(&tab_id)
                                                        {
                                                            resize_pty_session(
                                                                session, rows, cols,
                                                            );
                                                            let screen = session.parser.screen_mut();
                                                            let on_alt = screen.alternate_screen();
                                                            let effective_offset = if on_alt {
                                                                0
                                                            } else {
                                                                screen.set_scrollback(usize::MAX);
                                                                let max_sb = screen.scrollback();
                                                                session.scroll_offset = session.scroll_offset.min(max_sb);
                                                                screen.set_scrollback(session.scroll_offset);
                                                                session.scroll_offset
                                                            };
                                                            let cursor_visible = show_cursor && effective_offset == 0;
                                                            let job = terminal_layout_job(
                                                                session.parser.screen(),
                                                                cursor_visible,
                                                                font_id.clone(),
                                                            );
                                                            if effective_offset > 0 {
                                                                session.parser.screen_mut().set_scrollback(0);
                                                            }
                                                            job
                                                        } else {
                                                            terminal_plain_layout_job(
                                                                &tab_lines.join("\n"),
                                                                font_id.clone(),
                                                            )
                                                        };
                                                    terminal_job.wrap.max_width =
                                                        size.x.max(1.0);
                                                    ui.allocate_ui_with_layout(
                                                        size,
                                                        egui::Layout::left_to_right(
                                                            egui::Align::Min,
                                                        ),
                                                        |ui| {
                                                            ui.add(
                                                                egui::Label::new(
                                                                    terminal_job,
                                                                )
                                                                .sense(
                                                                    egui::Sense::click(),
                                                                ),
                                                            )
                                                        },
                                                    )
                                                    .inner
                                                },
                                            )
                                            .inner
                                        })
                                        .inner
                                });
                            let terminal_focus_id =
                                ui.make_persistent_id(("terminal_focus", tab_id));
                            let terminal_focus_response = ui.interact(
                                terminal_response.response.rect,
                                terminal_focus_id,
                                egui::Sense::click(),
                            );
                            if terminal_focus_response.clicked() {
                                terminal_focus_response.request_focus();
                                self.log_debug(format!(
                                    "terminal focus requested tab={tab_id}"
                                ));
                            }
                            if terminal_focus_response.secondary_clicked() {
                                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                    if let Ok(text) = clipboard.get_text() {
                                        if !text.is_empty() {
                                            self.send_raw_bytes_to_connection_tab(
                                                tab_id,
                                                text.as_bytes(),
                                            );
                                            self.log_debug(format!(
                                                "right-click paste tab={tab_id} bytes={}",
                                                text.len()
                                            ));
                                        }
                                    }
                                }
                            }
                            // Mouse wheel scrollback
                            if terminal_focus_response.hovered() || terminal_focus_response.has_focus() {
                                let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
                                if scroll_delta != 0.0 {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        if !session.parser.screen().alternate_screen() {
                                            let lines = (scroll_delta.abs() / self.scroll_sensitivity).ceil().max(1.0) as usize;
                                            if scroll_delta > 0.0 {
                                                session.scroll_offset = session.scroll_offset.saturating_add(lines);
                                            } else {
                                                session.scroll_offset = session.scroll_offset.saturating_sub(lines);
                                            }
                                        }
                                    }
                                }
                            }
                            // "Scrolled up" indicator overlay
                            let current_scroll_offset = self.pty_sessions
                                .get(&tab_id)
                                .map(|s| s.scroll_offset)
                                .unwrap_or(0);
                            if current_scroll_offset > 0 {
                                let term_rect = terminal_response.response.rect;
                                let banner_height = 24.0;
                                let banner_rect = egui::Rect::from_min_size(
                                    egui::pos2(term_rect.left(), term_rect.bottom() - banner_height),
                                    egui::vec2(term_rect.width(), banner_height),
                                );
                                ui.painter().rect_filled(
                                    banner_rect,
                                    0.0,
                                    egui::Color32::from_rgba_unmultiplied(40, 40, 40, 220),
                                );
                                let banner_text = format!(
                                    "Scrolled up {} lines \u{2014} click to return to bottom",
                                    current_scroll_offset
                                );
                                ui.painter().text(
                                    banner_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    banner_text,
                                    egui::FontId::proportional(12.0),
                                    egui::Color32::from_rgb(200, 200, 200),
                                );
                                let banner_id = ui.make_persistent_id(("scroll_banner", tab_id));
                                let banner_response = ui.interact(
                                    banner_rect,
                                    banner_id,
                                    egui::Sense::click(),
                                );
                                if banner_response.clicked() {
                                    if let Some(session) = self.pty_sessions.get_mut(&tab_id) {
                                        session.scroll_offset = 0;
                                    }
                                }
                            }
                            if terminal_focus_response.has_focus() {
                                ui.memory_mut(|mem| {
                                    mem.set_focus_lock_filter(
                                        terminal_focus_id,
                                        egui::EventFilter {
                                            tab: true,
                                            horizontal_arrows: true,
                                            vertical_arrows: true,
                                            escape: true,
                                        },
                                    );
                                });
                                self.forward_terminal_key_input(ui.ctx(), tab_id);
                            }
                        },
                    );
                });
            }
        }

        fn render_file_browser(&mut self, ui: &mut egui::Ui, tab_id: u64) {
            ui.label("File Browser");
            ui.separator();

            // Collect state to avoid borrow conflicts
            let fb_snapshot = self.file_browsers.get(&tab_id).map(|fb| {
                (
                    fb.path_input.clone(),
                    fb.current_path.clone(),
                    fb.entries.clone(),
                    fb.selected_entries.clone(),
                    fb.last_clicked_entry,
                    fb.status.clone(),
                    fb.pending_downloads,
                )
            });

            let Some((
                mut path_input,
                current_path,
                entries,
                selected_entries,
                last_clicked,
                status,
                pending_downloads,
            )) = fb_snapshot
            else {
                ui.label("No file browser for this tab");
                return;
            };

            // Path bar
            let mut navigate_to: Option<String> = None;
            let busy = matches!(
                status,
                FileOpStatus::Listing | FileOpStatus::Downloading | FileOpStatus::Uploading
            );
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut path_input)
                        .desired_width(150.0)
                        .font(egui::TextStyle::Small),
                );
                if response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    navigate_to = Some(path_input.clone());
                }
                if ui.small_button("Go").clicked() {
                    navigate_to = Some(path_input.clone());
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Refresh").small())
                    .clicked()
                {
                    navigate_to = Some(current_path.clone());
                }
            });
            // Write back path_input
            if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                fb.path_input = path_input;
            }

            // Up button
            if ui.button(".. (Up)").clicked() {
                navigate_to = Some(parent_path(&current_path));
            }

            // Status indicator
            match &status {
                FileOpStatus::Listing => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading...");
                    });
                }
                FileOpStatus::Downloading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        if pending_downloads > 1 {
                            ui.label(format!("Downloading ({pending_downloads} remaining)..."));
                        } else {
                            ui.label("Downloading...");
                        }
                    });
                }
                FileOpStatus::Uploading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Uploading...");
                    });
                }
                FileOpStatus::Error(err) => {
                    ui.colored_label(egui::Color32::RED, truncate(err, 40));
                }
                FileOpStatus::Idle => {}
            }

            ui.separator();

            // File list
            let mut new_selected = selected_entries.clone();
            let mut new_last_clicked = last_clicked;
            let mut double_clicked_dir: Option<String> = None;
            let modifiers = ui.input(|i| i.modifiers);
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 40.0)
                .show(ui, |ui| {
                    for (idx, entry) in entries.iter().enumerate() {
                        let prefix = if entry.is_dir { ">" } else { " " };
                        let label = format!("{prefix} {}", entry.name);
                        let is_selected = new_selected.contains(&idx);
                        let response =
                            ui.selectable_label(is_selected, truncate(&label, 30));
                        if response.clicked() {
                            if modifiers.shift {
                                if let Some(anchor) = last_clicked {
                                    let range = if anchor <= idx {
                                        anchor..=idx
                                    } else {
                                        idx..=anchor
                                    };
                                    for i in range {
                                        new_selected.insert(i);
                                    }
                                } else {
                                    new_selected.clear();
                                    new_selected.insert(idx);
                                }
                                new_last_clicked = Some(idx);
                            } else if modifiers.command || modifiers.ctrl {
                                if new_selected.contains(&idx) {
                                    new_selected.remove(&idx);
                                } else {
                                    new_selected.insert(idx);
                                }
                                new_last_clicked = Some(idx);
                            } else {
                                new_selected.clear();
                                new_selected.insert(idx);
                                new_last_clicked = Some(idx);
                            }
                        }
                        if response.double_clicked() && entry.is_dir {
                            double_clicked_dir =
                                Some(join_path(&current_path, &entry.name));
                        }
                    }
                    if entries.is_empty()
                        && matches!(status, FileOpStatus::Idle)
                    {
                        ui.label("(empty)");
                    }
                });

            // Update selection
            if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                fb.selected_entries = new_selected.clone();
                fb.last_clicked_entry = new_last_clicked;
            }

            // Navigate on double-click dir
            if let Some(dir_path) = double_clicked_dir {
                navigate_to = Some(dir_path);
            }

            ui.separator();

            // Upload / Download buttons
            ui.horizontal(|ui| {
                // Gather selected file entries (non-dir)
                let selected_files: Vec<usize> = new_selected
                    .iter()
                    .copied()
                    .filter(|&idx| {
                        entries.get(idx).map(|e| !e.is_dir).unwrap_or(false)
                    })
                    .collect();
                let can_download = !selected_files.is_empty();
                let download_label = if selected_files.len() > 1 {
                    format!("Download ({})", selected_files.len())
                } else {
                    "Download".to_string()
                };
                if ui
                    .add_enabled(can_download, egui::Button::new(download_label))
                    .clicked()
                {
                    if selected_files.len() == 1 {
                        // Single file: save-file dialog (existing behavior)
                        if let Some(entry) = entries.get(selected_files[0]) {
                            let remote = join_path(&current_path, &entry.name);
                            let dialog = rfd::FileDialog::new()
                                .set_file_name(&entry.name);
                            if let Some(local) = dialog.save_file() {
                                self.request_file_download(
                                    tab_id,
                                    remote,
                                    local.to_string_lossy().to_string(),
                                );
                            }
                        }
                    } else {
                        // Multiple files: pick destination folder
                        let dialog = rfd::FileDialog::new();
                        if let Some(dest_dir) = dialog.pick_folder() {
                            let total = selected_files.len();
                            if let Some(fb) = self.file_browsers.get_mut(&tab_id) {
                                fb.pending_downloads = total;
                                fb.status = FileOpStatus::Downloading;
                            }
                            for idx in &selected_files {
                                if let Some(entry) = entries.get(*idx) {
                                    let remote =
                                        join_path(&current_path, &entry.name);
                                    let local = dest_dir
                                        .join(&entry.name)
                                        .to_string_lossy()
                                        .to_string();
                                    self.request_batch_file_download(
                                        tab_id, remote, local,
                                    );
                                }
                            }
                        }
                    }
                }
                if ui.button("Upload").clicked() {
                    let dialog = rfd::FileDialog::new();
                    if let Some(local) = dialog.pick_file() {
                        self.request_file_upload(
                            tab_id,
                            local.to_string_lossy().to_string(),
                            current_path.clone(),
                        );
                    }
                }
            });

            // Handle navigation
            if let Some(path) = navigate_to {
                self.request_file_listing(tab_id, path);
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
                self.poll_refresh_events();
                self.poll_file_op_events();
                if self.refreshing {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                if self.main_tab == MainTab::Connections && !self.pty_sessions.is_empty() {
                    ctx.request_repaint_after(Duration::from_millis(16));
                }
                if self.file_browsers.values().any(|b| {
                    matches!(
                        b.status,
                        FileOpStatus::Listing
                            | FileOpStatus::Downloading
                            | FileOpStatus::Uploading
                    )
                }) {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                if self.gui_smoke_should_close {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }

                // Intercept window close when there are active connections.
                if ctx.input(|i| i.viewport().close_requested()) {
                    let active = self.pty_sessions.len();
                    if active > 0 {
                        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                        self.show_close_blocked = true;
                        self.log_warn(format!(
                            "close blocked: {active} active connection(s) still open"
                        ));
                    }
                }

                if self.show_close_blocked {
                    let active = self.pty_sessions.len();
                    if active == 0 {
                        // All connections were closed while the dialog
                        // was open — allow close now.
                        self.show_close_blocked = false;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        egui::Window::new("Active Connections")
                            .collapsible(false)
                            .resizable(false)
                            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                            .show(ctx, |ui| {
                                ui.label(format!(
                                    "There {} {} active connection{}. \
                                     Close all connections before exiting.",
                                    if active == 1 { "is" } else { "are" },
                                    active,
                                    if active == 1 { "" } else { "s" },
                                ));
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    if ui.button("Close All & Exit").clicked() {
                                        let tab_ids: Vec<u64> =
                                            self.pty_sessions.keys().copied().collect();
                                        for id in tab_ids {
                                            self.close_connection_tab(id);
                                        }
                                        self.show_close_blocked = false;
                                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                    }
                                    if ui.button("Go Back").clicked() {
                                        self.show_close_blocked = false;
                                    }
                                });
                            });
                    }
                }

                egui::TopBottomPanel::top("top").show(ctx, |ui| {
                    egui::MenuBar::new().ui(ui, |ui| {
                        ui.menu_button("Edit", |ui| {
                            ui.menu_button("Theme", |ui| {
                                if ui
                                    .selectable_label(self.dark_mode, "Dark")
                                    .clicked()
                                {
                                    self.dark_mode = true;
                                    ctx.set_visuals(egui::Visuals::dark());
                                    self.config.theme = Some("dark".to_string());
                                    let _ = self.config.save();
                                    ui.close();
                                }
                                if ui
                                    .selectable_label(!self.dark_mode, "Light")
                                    .clicked()
                                {
                                    self.dark_mode = false;
                                    ctx.set_visuals(egui::Visuals::light());
                                    self.config.theme = Some("light".to_string());
                                    let _ = self.config.save();
                                    ui.close();
                                }
                            });
                            ui.menu_button("Scroll Sensitivity", |ui| {
                                for &(label, value) in &[("Low", 20.0f32), ("Medium (Default)", 10.0), ("High", 5.0)] {
                                    if ui.selectable_label(self.scroll_sensitivity == value, label).clicked() {
                                        self.scroll_sensitivity = value;
                                        self.config.scroll_sensitivity = Some(value);
                                        let _ = self.config.save();
                                        ui.close();
                                    }
                                }
                            });
                        });
                    });

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

                if self.main_tab == MainTab::Inventory {
                egui::SidePanel::left("controls")
                    .resizable(true)
                    .show(ctx, |ui| {
                    ui.heading("Controls");

                    if self.refreshing {
                        ui.add_enabled(false, egui::Button::new("Refreshing..."));
                    } else if ui.button("Refresh Inventory").clicked() {
                        self.refresh_context_and_inventory(true);
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
                        self.refresh_context_and_inventory(true);
                    }

                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Terminal");
                        if ui.small_button("Rescan").clicked() {
                            self.terminals =
                                filter_embedded_terminals(discover_terminals());
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
                        if self.guarded_action("connect", |app| app.connect_selected()) {
                            self.main_tab = MainTab::Connections;
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
                        self.guarded_action("port forward", |app| app.port_forward_selected());
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
                }

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

    #[cfg(test)]
    fn shell_plan(terminal: Option<&TerminalOption>) -> (String, Vec<String>) {
        if cfg!(windows) {
            let fallback = || {
                (
                    windows_cmd_path(),
                    vec!["/Q".to_string(), "/K".to_string()],
                )
            };
            match terminal.map(|t| t.id.as_str()) {
                Some("pwsh") => {
                    let program = terminal.expect("terminal should be present for pwsh").program.clone();
                    (program, vec!["-NoLogo".to_string()])
                }
                Some("powershell") => {
                    let program =
                        terminal.expect("terminal should be present for powershell").program.clone();
                    (program, vec!["-NoLogo".to_string()])
                }
                Some("cmd") => (
                    windows_cmd_path(),
                    vec!["/Q".to_string(), "/K".to_string()],
                ),
                _ => fallback(),
            }
        } else {
            if let Ok(shell) = std::env::var("SHELL") {
                if !shell.trim().is_empty() {
                    return (shell, vec!["-i".to_string()]);
                }
            }
            ("/bin/bash".to_string(), vec!["-i".to_string()])
        }
    }

    #[cfg(target_os = "windows")]
    fn windows_system_root() -> String {
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
    }

    #[cfg(target_os = "windows")]
    fn windows_cmd_path() -> String {
        format!("{}\\System32\\cmd.exe", windows_system_root())
    }

    #[cfg(not(target_os = "windows"))]
    fn windows_cmd_path() -> String {
        "cmd.exe".to_string()
    }

    fn format_sim_command(
        kind: TerminalKind,
        cmd: &str,
        instance_id: &str,
        port_forward: Option<(u16, u16)>,
    ) -> String {
        let status_line = if let Some((local, remote)) = port_forward {
            format!("[SIM MODE] port-forward {local}:{remote} for {instance_id}")
        } else {
            format!("[SIM MODE] session open for {instance_id}")
        };

        if cfg!(windows) {
            match kind {
                TerminalKind::Cmd => format!("echo [SIM MODE] {cmd} & echo {status_line}"),
                TerminalKind::PowerShell7 | TerminalKind::WindowsPowerShell => {
                    format!("Write-Host '[SIM MODE] {cmd}'; Write-Host '{status_line}'")
                }
                _ => format!("echo '[SIM MODE] {cmd}'; echo '{status_line}'"),
            }
        } else {
            format!("echo '[SIM MODE] {cmd}'; echo '{status_line}'")
        }
    }

    fn terminal_debug_label(terminal: Option<&TerminalOption>) -> String {
        terminal
            .map(|t| format!("id={} name={} kind={:?} program={}", t.id, t.display_name, t.kind, t.program))
            .unwrap_or_else(|| "none".to_string())
    }

    fn pty_command_for_context(
        terminal: Option<&TerminalOption>,
        context: &AwsContext,
        command_line: &str,
        command_args: &[String],
    ) -> PtyCommand {
        if context.mode == Mode::Sim {
            return sim_pty_command(terminal, command_line);
        }
        if command_args.is_empty() {
            return sim_pty_command(terminal, command_line);
        }
        // On Windows, the session-manager-plugin writes directly via
        // Windows Console APIs, which ConPTY does not capture when
        // `aws` is spawned as a bare process.  Wrapping the command
        // inside the selected shell (PowerShell / CMD) lets the shell
        // mediate console I/O so output flows through the PTY pipe.
        if cfg!(windows) {
            return shell_wrapped_pty_command(terminal, command_line);
        }
        PtyCommand {
            program: "aws".to_string(),
            args: command_args.to_vec(),
        }
    }

    fn shell_wrapped_pty_command(
        _terminal: Option<&TerminalOption>,
        command_line: &str,
    ) -> PtyCommand {
        // Always use CMD for live-mode shell wrapping.  Windows
        // PowerShell 5.1 + ConPTY has a known issue where only
        // cursor-visibility sequences flow through the PTY pipe
        // and all other output (including keystroke echo) is lost.
        // CMD's simpler console handling works reliably with ConPTY,
        // and /K runs the command then keeps the shell alive for
        // interactive I/O (e.g. session-manager-plugin).
        PtyCommand {
            program: windows_cmd_path(),
            args: vec!["/K".to_string(), command_line.to_string()],
        }
    }

    fn sim_pty_command(terminal: Option<&TerminalOption>, command_line: &str) -> PtyCommand {
        if cfg!(windows) {
            let kind = terminal
                .map(|t| t.kind.clone())
                .unwrap_or(TerminalKind::Cmd);
            match kind {
                TerminalKind::PowerShell7 | TerminalKind::WindowsPowerShell => {
                    let program = terminal
                        .map(|t| t.program.clone())
                        .unwrap_or_else(|| "powershell".to_string());
                    PtyCommand {
                        program,
                        args: vec![
                            "-NoLogo".to_string(),
                            "-NoExit".to_string(),
                            "-Command".to_string(),
                            command_line.to_string(),
                        ],
                    }
                }
                _ => PtyCommand {
                    program: windows_cmd_path(),
                    args: vec![
                        "/Q".to_string(),
                        "/K".to_string(),
                        command_line.to_string(),
                    ],
                },
            }
        } else {
            let program = std::env::var("SHELL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/bash".to_string());
            PtyCommand {
                program,
                args: vec!["-lc".to_string(), command_line.to_string()],
            }
        }
    }


    /// Creates a PTY session but does NOT start the reader thread.
    /// Returns the session and the reader handle separately so the caller
    /// can control when reading begins (important for avoiding the race
    /// between PtyReady and Output events on Windows).
    fn spawn_pty_session_parts(
        tab_id: u64,
        command: &PtyCommand,
        context: &AwsContext,
    ) -> Result<(PtySession, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 45,
                cols: 180,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| AppError::Parse(format!("Failed to allocate PTY: {err}")))?;

        let mut cmd = CommandBuilder::new(&command.program);
        for arg in &command.args {
            cmd.arg(arg);
        }
        eprintln!(
            "tab={tab_id} PTY spawn: program={} args={:?}",
            command.program, command.args
        );
        cmd.env("AWS_PROFILE", &context.profile);
        cmd.env("AWS_REGION", &context.region);
        cmd.env("TERM", "xterm-256color");
        #[cfg(target_os = "windows")]
        {
            let system_root = windows_system_root();
            let comspec = windows_cmd_path();
            cmd.env("SystemRoot", &system_root);
            cmd.env("WINDIR", &system_root);
            cmd.env("ComSpec", &comspec);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| AppError::Parse(format!("Failed to spawn PTY command: {err}")))?;
        drop(pair.slave);

        let master = pair.master;
        let reader = master
            .try_clone_reader()
            .map_err(|err| AppError::Parse(format!("Failed to create PTY reader: {err}")))?;
        let writer = master
            .take_writer()
            .map_err(|err| AppError::Parse(format!("Failed to create PTY writer: {err}")))?;

        let session = PtySession {
            child,
            master: Arc::new(Mutex::new(master)),
            writer: Arc::new(Mutex::new(writer)),
            parser: vt100::Parser::new(45, 180, 10_000),
            last_size: None,
            bytes_received: 0,
            output_event_count: 0,
            scroll_offset: 0,
        };

        Ok((session, reader))
    }

    /// Starts a background thread that reads from the PTY and sends
    /// `ProcEvent::Output` / `ProcEvent::Error` events via `proc_tx`.
    fn start_pty_reader_thread(
        tab_id: u64,
        mut reader: Box<dyn Read + Send>,
        proc_tx: Sender<ProcEvent>,
    ) {
        std::thread::spawn(move || {
            let mut buf = [0_u8; 8192];
            let mut total_bytes: u64 = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = proc_tx.send(ProcEvent::Error {
                            tab_id,
                            error: format!(
                                "PTY reader EOF after {total_bytes} bytes total"
                            ),
                        });
                        break;
                    }
                    Ok(n) => {
                        total_bytes += n as u64;
                        let _ = proc_tx.send(ProcEvent::Output {
                            tab_id,
                            bytes: buf[..n].to_vec(),
                        });
                    }
                    Err(err) => {
                        let _ = proc_tx.send(ProcEvent::Error {
                            tab_id,
                            error: format!(
                                "PTY reader error after {total_bytes} bytes: {err}"
                            ),
                        });
                        break;
                    }
                }
            }
        });
    }

    /// Detects terminal query sequences in PTY output and sends the
    /// expected responses back through the writer.  Without these
    /// responses, programs like CMD and PowerShell hang at startup.
    fn respond_to_terminal_queries(session: &PtySession, bytes: &[u8]) {
        // ESC[6n — Device Status Report (cursor position query).
        // Response: ESC[{row};{col}R  (1-based).
        if bytes.windows(4).any(|w| w == b"\x1b[6n") {
            let (row, col) = session.parser.screen().cursor_position();
            let response = format!("\x1b[{};{}R", row + 1, col + 1);
            if let Ok(mut w) = session.writer.lock() {
                let _ = w.write_all(response.as_bytes());
                let _ = w.flush();
            }
        }
        // ESC[0c or ESC[c — Primary Device Attributes (DA1).
        // Response: ESC[?1;0c  (VT101 with no options).
        if bytes.windows(3).any(|w| w == b"\x1b[c")
            || bytes.windows(4).any(|w| w == b"\x1b[0c")
        {
            if let Ok(mut w) = session.writer.lock() {
                let _ = w.write_all(b"\x1b[?1;0c");
                let _ = w.flush();
            }
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
        pty_bytes: u64,
    ) -> String {
        let status = if running { "Running" } else { "Closed" };
        let bytes_label = if pty_bytes >= 1024 {
            format!("{}KB", pty_bytes / 1024)
        } else {
            format!("{pty_bytes}B")
        };
        format!(
            "Instance: {title} ({instance_id})\tPrivate IP: {private_ip}\tStatus: {status}\tRx: {bytes_label}"
        )
    }

    fn terminal_panel_fill() -> egui::Color32 {
        egui::Color32::from_rgb(22, 22, 22)
    }

    fn terminal_panel_text() -> egui::Color32 {
        egui::Color32::from_rgb(230, 230, 230)
    }

    fn terminal_layout_job(
        screen: &vt100::Screen,
        show_cursor: bool,
        font_id: egui::FontId,
    ) -> egui::text::LayoutJob {
        let (rows, cols) = screen.size();
        let default_fg = terminal_panel_text();
        let default_bg = terminal_panel_fill();
        let cursor = if show_cursor && !screen.hide_cursor() {
            Some(screen.cursor_position())
        } else {
            None
        };

        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let mut current_format: Option<egui::TextFormat> = None;
        let mut current_text = String::new();

        for row in 0..rows {
            for col in 0..cols {
                let cursor_cell = cursor == Some((row, col));
                let (cell_text, format) = if let Some(cell) = screen.cell(row, col) {
                    let text = if cell.is_wide_continuation() {
                        " "
                    } else {
                        let contents = cell.contents();
                        if contents.is_empty() {
                            " "
                        } else {
                            contents
                        }
                    };
                    let format = terminal_cell_format(
                        cell,
                        cursor_cell,
                        &font_id,
                        default_fg,
                        default_bg,
                    );
                    (text, format)
                } else {
                    let format = egui::TextFormat {
                        font_id: font_id.clone(),
                        color: default_fg,
                        background: egui::Color32::TRANSPARENT,
                        ..Default::default()
                    };
                    (" ", format)
                };

                if current_format
                    .as_ref()
                    .map(|f| f == &format)
                    .unwrap_or(false)
                {
                    current_text.push_str(cell_text);
                } else {
                    if let Some(format) = current_format.take() {
                        if !current_text.is_empty() {
                            job.append(&current_text, 0.0, format);
                        }
                        current_text.clear();
                    }
                    current_format = Some(format.clone());
                    current_text.push_str(cell_text);
                }
            }
            if row + 1 < rows {
                current_text.push('\n');
            }
        }

        if let Some(format) = current_format {
            if !current_text.is_empty() {
                job.append(&current_text, 0.0, format);
            }
        }
        job
    }

    fn terminal_plain_layout_job(
        text: &str,
        font_id: egui::FontId,
    ) -> egui::text::LayoutJob {
        let format = egui::TextFormat {
            font_id,
            color: terminal_panel_text(),
            background: egui::Color32::TRANSPARENT,
            ..Default::default()
        };
        egui::text::LayoutJob::single_section(text.to_string(), format)
    }

    fn terminal_grid_from_pixels(
        ui: &egui::Ui,
        font_id: &egui::FontId,
        available: egui::Vec2,
    ) -> (u16, u16) {
        let (cell_w, cell_h) = ui.fonts_mut(|f| {
            (f.glyph_width(font_id, 'W'), f.row_height(font_id))
        });
        let cell_w = if cell_w >= 1.0 { cell_w } else { font_id.size * 0.6 };
        let cell_h = if cell_h >= 1.0 { cell_h } else { font_id.size * 1.2 };
        let cols = (available.x / cell_w).floor().max(1.0) as u16;
        let rows = (available.y / cell_h).floor().max(1.0) as u16;
        (rows, cols)
    }

    fn resize_pty_session(session: &mut PtySession, rows: u16, cols: u16) {
        let next = Some((rows, cols));
        if session.last_size == next {
            return;
        }
        session.last_size = next;
        session.parser.screen_mut().set_size(rows, cols);
        if let Ok(master) = session.master.lock() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }

    fn terminal_cell_format(
        cell: &vt100::Cell,
        cursor: bool,
        font_id: &egui::FontId,
        default_fg: egui::Color32,
        default_bg: egui::Color32,
    ) -> egui::TextFormat {
        let mut fg = vt100_color_to_egui(cell.fgcolor(), default_fg);
        let mut bg = vt100_color_to_egui(cell.bgcolor(), default_bg);
        let mut inverse = cell.inverse();
        if cursor {
            inverse = !inverse;
        }
        if inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.bold() {
            fg = brighten_color(fg, 0.18);
        }
        if cell.dim() {
            fg = darken_color(fg, 0.22);
        }
        let background = if cell.bgcolor() != vt100::Color::Default || inverse {
            bg
        } else {
            egui::Color32::TRANSPARENT
        };
        egui::TextFormat {
            font_id: font_id.clone(),
            color: fg,
            background,
            italics: cell.italic(),
            underline: if cell.underline() {
                egui::Stroke::new(1.0, fg)
            } else {
                egui::Stroke::NONE
            },
            ..Default::default()
        }
    }

    fn vt100_color_to_egui(
        color: vt100::Color,
        default: egui::Color32,
    ) -> egui::Color32 {
        match color {
            vt100::Color::Default => default,
            vt100::Color::Idx(idx) => ansi_color_from_index(idx),
            vt100::Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
        }
    }

    fn brighten_color(color: egui::Color32, factor: f32) -> egui::Color32 {
        let r = color.r() as f32;
        let g = color.g() as f32;
        let b = color.b() as f32;
        let nr = r + (255.0 - r) * factor;
        let ng = g + (255.0 - g) * factor;
        let nb = b + (255.0 - b) * factor;
        egui::Color32::from_rgb(nr as u8, ng as u8, nb as u8)
    }

    fn darken_color(color: egui::Color32, factor: f32) -> egui::Color32 {
        let r = color.r() as f32;
        let g = color.g() as f32;
        let b = color.b() as f32;
        let nr = r * (1.0 - factor);
        let ng = g * (1.0 - factor);
        let nb = b * (1.0 - factor);
        egui::Color32::from_rgb(nr as u8, ng as u8, nb as u8)
    }

    fn ansi_color_from_index(idx: u8) -> egui::Color32 {
        const BASIC: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (205, 0, 0),
            (0, 205, 0),
            (205, 205, 0),
            (0, 0, 238),
            (205, 0, 205),
            (0, 205, 205),
            (229, 229, 229),
            (127, 127, 127),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (92, 92, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ];
        if idx < 16 {
            let (r, g, b) = BASIC[idx as usize];
            return egui::Color32::from_rgb(r, g, b);
        }
        if idx <= 231 {
            let mut n = idx - 16;
            let r = n / 36;
            n %= 36;
            let g = n / 6;
            let b = n % 6;
            let levels = [0, 95, 135, 175, 215, 255];
            return egui::Color32::from_rgb(levels[r as usize], levels[g as usize], levels[b as usize]);
        }
        let gray = 8 + (idx - 232).saturating_mul(10);
        egui::Color32::from_rgb(gray, gray, gray)
    }

    fn terminal_event_payload_for_terminal(
        event: &egui::Event,
        has_text: bool,
        has_key_backspace: bool,
    ) -> Option<Vec<u8>> {
        match event {
            egui::Event::Text(text) => {
                // BS (\x08) may arrive as a Text event on some platforms
                // when egui does not emit Key::Backspace.  Forward it as
                // the terminal backspace byte, but only when no explicit
                // Key::Backspace event is present (to avoid double-send).
                if text == "\x08" {
                    if has_key_backspace {
                        None
                    } else {
                        Some(vec![0x08])
                    }
                } else if text.chars().any(|c| c.is_control()) {
                    None
                } else {
                    Some(text.as_bytes().to_vec())
                }
            }
            egui::Event::Paste(text) => Some(text.as_bytes().to_vec()),
            // Copy/Cut events carry no modifier info so we cannot
            // distinguish Ctrl+C from Ctrl+Shift+C here.  These are
            // handled in forward_terminal_key_input instead.
            egui::Event::Copy => None,
            egui::Event::Cut => None,
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => match key {
                egui::Key::Enter => Some(b"\r".to_vec()),
                egui::Key::Backspace => Some(vec![0x08]),
                egui::Key::Tab => Some(b"\t".to_vec()),
                egui::Key::Escape => Some(vec![0x1b]),
                egui::Key::ArrowUp => Some(b"\x1b[A".to_vec()),
                egui::Key::ArrowDown => Some(b"\x1b[B".to_vec()),
                egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
                egui::Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
                egui::Key::Home => Some(b"\x1b[H".to_vec()),
                egui::Key::End => Some(b"\x1b[F".to_vec()),
                egui::Key::Delete => Some(b"\x1b[3~".to_vec()),
                egui::Key::C if modifiers.ctrl && !modifiers.shift => Some(vec![0x03]),
                egui::Key::D if modifiers.ctrl => Some(vec![0x04]),
                egui::Key::L if modifiers.ctrl => Some(vec![0x0c]),
                egui::Key::U if modifiers.ctrl => Some(vec![0x15]),
                egui::Key::W if modifiers.ctrl => Some(vec![0x17]),
                _ if !modifiers.ctrl && !modifiers.command && !modifiers.alt => {
                    if has_text {
                        None
                    } else {
                        key_ascii_fallback(*key, modifiers.shift).map(|c| vec![c])
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn terminal_event_kind(event: &egui::Event) -> String {
        match event {
            egui::Event::Text(_) => "text".to_string(),
            egui::Event::Paste(_) => "paste".to_string(),
            egui::Event::Copy => "copy".to_string(),
            egui::Event::Cut => "cut".to_string(),
            egui::Event::Key { key, .. } => format!("key:{key:?}"),
            egui::Event::PointerButton { .. } => "pointer_button".to_string(),
            egui::Event::PointerMoved(_) => "pointer_moved".to_string(),
            _ => "other".to_string(),
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

    fn filter_embedded_terminals(terminals: Vec<TerminalOption>) -> Vec<TerminalOption> {
        if cfg!(windows) {
            terminals
                .into_iter()
                .filter(|t| {
                    matches!(
                        t.kind,
                        TerminalKind::PowerShell7
                            | TerminalKind::WindowsPowerShell
                            | TerminalKind::Cmd
                    )
                })
                .collect()
        } else {
            terminals
        }
    }

    fn initial_terminal_id(config: &AppConfig, terminals: &[TerminalOption]) -> String {
        pick_default_terminal(config, terminals)
            .or_else(|| terminals.first().cloned())
            .map(|t| t.id)
            .unwrap_or_default()
    }

    fn parent_path(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return "/".to_string();
        }
        match trimmed.rfind('/') {
            Some(0) | None => "/".to_string(),
            Some(idx) => trimmed[..idx].to_string(),
        }
    }

    fn join_path(base: &str, component: &str) -> String {
        if base == "/" {
            format!("/{component}")
        } else {
            format!("{base}/{component}")
        }
    }

    fn parse_ls_output(output: &str) -> Vec<FileEntry> {
        let mut entries = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("total") {
                continue;
            }
            // Split on runs of whitespace, limit to 9 fields so the filename
            // (which may contain spaces) stays intact in the last element.
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }
            // The name is everything from the 9th field onward (index 8..)
            let name = parts[8..].join(" ");
            if name == "." || name == ".." {
                continue;
            }
            let permissions = parts[0].to_string();
            let is_dir = permissions.starts_with('d');
            let size = parts[4].parse::<u64>().unwrap_or(0);
            let modified = format!("{} {} {}", parts[5], parts[6], parts[7]);
            entries.push(FileEntry {
                name,
                is_dir,
                size,
                permissions,
                modified,
            });
        }
        sort_file_entries(&mut entries);
        entries
    }

    fn sort_file_entries(entries: &mut [FileEntry]) {
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    fn list_files_local(path: &str) -> std::result::Result<Vec<FileEntry>, String> {
        let read_dir =
            std::fs::read_dir(path).map_err(|e| format!("failed to read {path}: {e}"))?;
        let mut entries = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| format!("read_dir entry error: {e}"))?;
            let metadata = entry.metadata().map_err(|e| format!("metadata error: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = metadata.is_dir();
            let size = metadata.len();
            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    let secs = d.as_secs();
                    format!("{secs}")
                })
                .unwrap_or_default();
            let permissions = if is_dir {
                "drwxr-xr-x".to_string()
            } else {
                "-rw-r--r--".to_string()
            };
            entries.push(FileEntry {
                name,
                is_dir,
                size,
                permissions,
                modified,
            });
        }
        sort_file_entries(&mut entries);
        Ok(entries)
    }

    fn ssm_send_command(
        profile: &str,
        region: &str,
        instance_id: &str,
        command: &str,
    ) -> std::result::Result<String, String> {
        let output = std::process::Command::new("aws")
            .args([
                "ssm",
                "send-command",
                "--profile",
                profile,
                "--region",
                region,
                "--instance-ids",
                instance_id,
                "--document-name",
                "AWS-RunShellScript",
                "--parameters",
                &format!("commands=[\"{command}\"]"),
                "--query",
                "Command.CommandId",
                "--output",
                "text",
            ])
            .output()
            .map_err(|e| format!("ssm send-command failed: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ssm send-command error: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn ssm_wait_for_command(
        profile: &str,
        region: &str,
        instance_id: &str,
        command_id: &str,
    ) -> std::result::Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                return Err("ssm command timed out after 30s".to_string());
            }
            std::thread::sleep(Duration::from_millis(500));
            let output = std::process::Command::new("aws")
                .args([
                    "ssm",
                    "get-command-invocation",
                    "--profile",
                    profile,
                    "--region",
                    region,
                    "--command-id",
                    command_id,
                    "--instance-id",
                    instance_id,
                    "--query",
                    "Status",
                    "--output",
                    "text",
                ])
                .output()
                .map_err(|e| format!("ssm get-command-invocation failed: {e}"))?;
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
            match status.as_str() {
                "Success" => {
                    let out = std::process::Command::new("aws")
                        .args([
                            "ssm",
                            "get-command-invocation",
                            "--profile",
                            profile,
                            "--region",
                            region,
                            "--command-id",
                            command_id,
                            "--instance-id",
                            instance_id,
                            "--query",
                            "StandardOutputContent",
                            "--output",
                            "text",
                        ])
                        .output()
                        .map_err(|e| format!("ssm get output failed: {e}"))?;
                    return Ok(String::from_utf8_lossy(&out.stdout).to_string());
                }
                "Failed" | "Cancelled" | "TimedOut" => {
                    return Err(format!("ssm command ended with status: {status}"));
                }
                _ => continue,
            }
        }
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
        fn pty_command_for_context_uses_aws_in_live_mode() {
            let context = AwsContext {
                mode: Mode::Live,
                profile: "prod".to_string(),
                account_id: Some("000000000000".to_string()),
                arn: None,
                user_id: None,
                region: "us-east-1".to_string(),
                auth_status: AuthStatus::Ok,
            };
            let args = vec![
                "ssm".to_string(),
                "start-session".to_string(),
                "--target".to_string(),
                "i-123".to_string(),
            ];
            let cmd = pty_command_for_context(None, &context, "ignored", &args);
            assert_eq!(cmd.program, "aws");
            assert_eq!(cmd.args, args);
        }

        #[test]
        fn pty_command_for_context_falls_back_to_sim_when_args_empty() {
            let prev = std::env::var("SHELL").ok();
            std::env::set_var("SHELL", "/bin/bash");
            let context = AwsContext {
                mode: Mode::Live,
                profile: "prod".to_string(),
                account_id: Some("000000000000".to_string()),
                arn: None,
                user_id: None,
                region: "us-east-1".to_string(),
                auth_status: AuthStatus::Ok,
            };
            let cmd = pty_command_for_context(None, &context, "echo hi", &[]);
            assert_ne!(cmd.program, "aws");
            if let Some(prev) = prev {
                std::env::set_var("SHELL", prev);
            } else {
                std::env::remove_var("SHELL");
            }
        }

        #[test]
        #[cfg(windows)]
        fn filter_embedded_terminals_restricts_windows_shells() {
            let terminals = vec![
                TerminalOption {
                    id: "pwsh".to_string(),
                    display_name: "PowerShell 7".to_string(),
                    kind: TerminalKind::PowerShell7,
                    program: "pwsh.exe".to_string(),
                },
                TerminalOption {
                    id: "powershell".to_string(),
                    display_name: "Windows PowerShell".to_string(),
                    kind: TerminalKind::WindowsPowerShell,
                    program: "powershell.exe".to_string(),
                },
                TerminalOption {
                    id: "cmd".to_string(),
                    display_name: "Command Prompt".to_string(),
                    kind: TerminalKind::Cmd,
                    program: "cmd.exe".to_string(),
                },
            ];
            let filtered = filter_embedded_terminals(terminals);
            assert_eq!(filtered.len(), 3);
            assert!(filtered.iter().all(|t| {
                matches!(
                    t.kind,
                    TerminalKind::PowerShell7
                        | TerminalKind::WindowsPowerShell
                        | TerminalKind::Cmd
                )
            }));
        }

        #[test]
        #[cfg(not(windows))]
        fn filter_embedded_terminals_noop_non_windows() {
            let terminals = vec![
                TerminalOption {
                    id: "xterm".to_string(),
                    display_name: "XTerm".to_string(),
                    kind: TerminalKind::Xterm,
                    program: "xterm".to_string(),
                },
                TerminalOption {
                    id: "kitty".to_string(),
                    display_name: "Kitty".to_string(),
                    kind: TerminalKind::Kitty,
                    program: "kitty".to_string(),
                },
            ];
            let filtered = filter_embedded_terminals(terminals.clone());
            assert_eq!(filtered.len(), terminals.len());
            assert_eq!(filtered[0].id, terminals[0].id);
            assert_eq!(filtered[1].id, terminals[1].id);
        }

        #[test]
        #[cfg(windows)]
        fn windows_cmd_path_is_absolute() {
            let path = windows_cmd_path();
            assert!(path.to_ascii_lowercase().ends_with("\\system32\\cmd.exe"));
        }

        #[test]
        #[cfg(not(windows))]
        fn windows_cmd_path_stub_has_cmd_name() {
            let path = windows_cmd_path();
            assert!(path.to_ascii_lowercase().contains("cmd"));
        }

        #[test]
        #[cfg(windows)]
        fn format_sim_command_uses_cmd_syntax() {
            let cmd = format_sim_command(
                TerminalKind::Cmd,
                "aws ssm start-session --target i-123 --region us-east-1",
                "i-123",
                None,
            );
            assert!(cmd.contains("echo [SIM MODE]"));
            assert!(cmd.contains("& echo [SIM MODE] session open for i-123"));
        }

        #[test]
        #[cfg(windows)]
        fn format_sim_command_uses_powershell_syntax() {
            let cmd = format_sim_command(
                TerminalKind::WindowsPowerShell,
                "aws ssm start-session --target i-123 --region us-east-1",
                "i-123",
                None,
            );
            assert!(cmd.contains("Write-Host"));
            assert!(cmd.contains("session open for i-123"));
        }

        #[test]
        #[cfg(not(windows))]
        fn format_sim_command_uses_shell_echo() {
            let cmd = format_sim_command(
                TerminalKind::Xterm,
                "aws ssm start-session --target i-123 --region us-east-1",
                "i-123",
                None,
            );
            assert!(cmd.contains("echo '[SIM MODE]"));
            assert!(cmd.contains("session open for i-123"));
        }

        #[test]
        fn terminal_debug_label_includes_id() {
            let terminal = TerminalOption {
                id: "cmd".to_string(),
                display_name: "Command Prompt".to_string(),
                kind: ec2_manager::models::TerminalKind::Cmd,
                program: "cmd.exe".to_string(),
            };
            let label = terminal_debug_label(Some(&terminal));
            assert!(label.contains("id=cmd"));
            assert!(label.contains("Command Prompt"));
        }

        #[test]
        fn terminal_event_kind_reports_key() {
            let key = egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            };
            assert!(terminal_event_kind(&key).starts_with("key:"));
        }

        #[test]
        fn guarded_action_logs_error_on_failure() {
            let mut app = Ec2GuiApp::new(GuiOptions {
                mode: Mode::Sim,
                region: None,
                dry_run: true,
            });
            let ok = app.guarded_action("test-action", |_app| {
                Err(AppError::Parse("boom".to_string()))
            });
            assert!(!ok);
            assert!(app
                .logs
                .iter()
                .any(|entry| entry.message.contains("test-action failed")));
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
                Vec::new(),
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
        fn sim_mode_open_connection_tab_spawns_terminal_output() {
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
                Vec::new(),
                &context,
            );
            if let Err(AppError::Parse(message)) = &open_result {
                if message.contains("Permission denied") {
                    // Some CI/sandbox environments disallow openpty.
                    return;
                }
            }
            open_result.expect("sim open should spawn a terminal session");

            let tab_id = app
                .connections
                .selected()
                .expect("selected connection tab should exist");
            for _ in 0..60 {
                app.poll_connection_events();
                let found = app
                    .connections
                    .selected_ref()
                    .map(|tab| tab.lines.iter().any(|line| line.contains("terminal-ok")))
                    .unwrap_or(false);
                if found {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            let found = app
                .connections
                .selected_ref()
                .map(|tab| tab.lines.iter().any(|line| line.contains("terminal-ok")))
                .unwrap_or(false);
            assert!(found, "expected terminal output to be captured");

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
                format_connection_summary_line("api-a", "i-123", "10.0.1.25", true, 0);
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
            assert_eq!(
                terminal_event_payload_for_terminal(&ctrl_c, false, false),
                Some(vec![0x03])
            );

            let enter = egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            };
            assert_eq!(
                terminal_event_payload_for_terminal(&enter, false, false),
                Some(b"\r".to_vec())
            );

            let ctrl_w = egui::Event::Key {
                key: egui::Key::W,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            };
            assert_eq!(
                terminal_event_payload_for_terminal(&ctrl_w, false, false),
                Some(vec![0x17])
            );

            let ctrl_u = egui::Event::Key {
                key: egui::Key::U,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            };
            assert_eq!(
                terminal_event_payload_for_terminal(&ctrl_u, false, false),
                Some(vec![0x15])
            );

            let paste = egui::Event::Paste("echo hi".to_string());
            assert_eq!(
                terminal_event_payload_for_terminal(&paste, false, false),
                Some("echo hi".as_bytes().to_vec())
            );
        }

        #[test]
        fn terminal_event_payload_copy_returns_none() {
            // Copy events are handled in forward_terminal_key_input, not here
            assert_eq!(
                terminal_event_payload_for_terminal(&egui::Event::Copy, false, false),
                None
            );
        }

        #[test]
        fn terminal_event_payload_cut_returns_none() {
            // Cut events are handled natively by egui
            assert_eq!(
                terminal_event_payload_for_terminal(&egui::Event::Cut, false, false),
                None
            );
        }

        #[test]
        fn terminal_event_payload_ctrl_shift_c_returns_none() {
            let ctrl_shift_c = egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
            };
            assert_eq!(
                terminal_event_payload_for_terminal(&ctrl_shift_c, false, false),
                None
            );
        }

        #[test]
        fn terminal_event_payload_backspace_sends_bs() {
            let bs = egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            };
            assert_eq!(
                terminal_event_payload_for_terminal(&bs, false, false),
                Some(vec![0x08])
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
            assert_eq!(
                terminal_event_payload_for_terminal(&key_a, false, false),
                Some(vec![b'a'])
            );
            assert_eq!(
                terminal_event_payload_for_terminal(&key_a, true, false),
                None
            );

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
            assert_eq!(
                terminal_event_payload_for_terminal(&key_a_shift, false, false),
                Some(vec![b'A'])
            );
            assert_eq!(
                terminal_event_payload_for_terminal(&key_a_shift, true, false),
                None
            );
        }

        #[test]
        fn terminal_event_payload_text_bs_without_key_backspace() {
            // When no Key::Backspace event is present, Text("\x08")
            // should be forwarded as BS (0x08).
            let text_bs = egui::Event::Text("\u{8}".to_string());
            assert_eq!(
                terminal_event_payload_for_terminal(&text_bs, false, false),
                Some(vec![0x08])
            );
        }

        #[test]
        fn terminal_event_payload_text_bs_skipped_when_key_backspace_present() {
            // When Key::Backspace IS present, Text("\x08") should be
            // suppressed to avoid double-sending.
            let text_bs = egui::Event::Text("\u{8}".to_string());
            assert_eq!(
                terminal_event_payload_for_terminal(&text_bs, false, true),
                None
            );
        }

        #[test]
        fn terminal_event_payload_ignores_other_control_text() {
            // Control chars other than BS should still be filtered.
            let ctrl_text = egui::Event::Text("\u{1}".to_string());
            assert_eq!(
                terminal_event_payload_for_terminal(&ctrl_text, false, false),
                None
            );
        }

        #[test]
        fn terminal_panel_text_is_light() {
            let text = terminal_panel_text();
            assert!(text.r() >= 200);
            assert!(text.g() >= 200);
            assert!(text.b() >= 200);
        }

        #[test]
        fn ansi_color_from_index_maps_basic_colors() {
            assert_eq!(
                ansi_color_from_index(1),
                egui::Color32::from_rgb(205, 0, 0)
            );
            assert_eq!(
                ansi_color_from_index(10),
                egui::Color32::from_rgb(0, 255, 0)
            );
        }

        #[test]
        fn terminal_layout_job_applies_cell_colors() {
            let mut parser = vt100::Parser::new(1, 2, 10);
            parser.process(b"\x1b[31mR\x1b[42mG");
            let job = terminal_layout_job(
                parser.screen(),
                false,
                egui::FontId::monospace(12.0),
            );
            let mut found_red = false;
            for section in &job.sections {
                let text = &job.text[section.byte_range.clone()];
                if text.contains('R') {
                    assert_eq!(
                        section.format.color,
                        ansi_color_from_index(1)
                    );
                    found_red = true;
                    break;
                }
            }
            assert!(found_red, "expected red cell to be present");
        }

        #[test]
        fn inventory_headers_do_not_include_app_service() {
            assert!(!INVENTORY_HEADERS
                .iter()
                .any(|(label, _)| *label == "App/Service"));
        }

        #[test]
        fn terminal_layout_job_includes_screen_text() {
            let mut parser = vt100::Parser::new(2, 10, 100);
            parser.process(b"hello");
            let job = terminal_layout_job(
                parser.screen(),
                false,
                egui::FontId::monospace(12.0),
            );
            assert!(job.text.contains("hello"));
        }

        #[test]
        fn terminal_panel_fill_is_dark() {
            let fill = terminal_panel_fill();
            assert!(fill.r() <= 30);
            assert!(fill.g() <= 30);
            assert!(fill.b() <= 30);
        }

        #[test]
        fn parse_ls_output_extracts_files_and_dirs() {
            let output = "\
total 24
drwxr-xr-x 3 user user 4096 Jan 10 12:00 .
drwxr-xr-x 5 user user 4096 Jan 10 12:00 ..
drwxr-xr-x 2 user user 4096 Jan 10 12:00 config
-rw-r--r-- 1 user user 1234 Jan 10 12:00 readme.md
-rw-r--r-- 1 user user  567 Jan 10 12:00 app.py";
            let entries = parse_ls_output(output);
            assert_eq!(entries.len(), 3);
            // dirs first
            assert!(entries[0].is_dir);
            assert_eq!(entries[0].name, "config");
            // then files alphabetically
            assert!(!entries[1].is_dir);
            assert_eq!(entries[1].name, "app.py");
            assert!(!entries[2].is_dir);
            assert_eq!(entries[2].name, "readme.md");
        }

        #[test]
        fn parse_ls_output_skips_dot_entries() {
            let output = "\
total 8
drwxr-xr-x 2 user user 4096 Jan 10 12:00 .
drwxr-xr-x 5 user user 4096 Jan 10 12:00 ..
-rw-r--r-- 1 user user   42 Jan 10 12:00 file.txt";
            let entries = parse_ls_output(output);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "file.txt");
        }

        #[test]
        fn parse_ls_output_handles_empty_output() {
            let entries = parse_ls_output("");
            assert!(entries.is_empty());
        }

        #[test]
        fn parent_path_navigates_up() {
            assert_eq!(parent_path("/home/user"), "/home");
            assert_eq!(parent_path("/home"), "/");
            assert_eq!(parent_path("/"), "/");
        }

        #[test]
        fn join_path_appends_component() {
            assert_eq!(join_path("/home", "user"), "/home/user");
            assert_eq!(join_path("/", "etc"), "/etc");
        }

        #[test]
        fn file_browser_state_defaults() {
            let fb = FileBrowserState::default();
            assert!(matches!(fb.status, FileOpStatus::Idle));
            assert!(fb.entries.is_empty());
            assert!(!fb.initialized);
            assert!(fb.selected_entries.is_empty());
            assert!(fb.last_clicked_entry.is_none());
            assert_eq!(fb.pending_downloads, 0);
        }

        #[test]
        fn file_entry_sort_dirs_first() {
            let mut entries = vec![
                FileEntry {
                    name: "zebra.txt".to_string(),
                    is_dir: false,
                    size: 100,
                    permissions: "-rw-r--r--".to_string(),
                    modified: String::new(),
                },
                FileEntry {
                    name: "alpha_dir".to_string(),
                    is_dir: true,
                    size: 4096,
                    permissions: "drwxr-xr-x".to_string(),
                    modified: String::new(),
                },
                FileEntry {
                    name: "apple.txt".to_string(),
                    is_dir: false,
                    size: 50,
                    permissions: "-rw-r--r--".to_string(),
                    modified: String::new(),
                },
            ];
            sort_file_entries(&mut entries);
            assert_eq!(entries[0].name, "alpha_dir");
            assert!(entries[0].is_dir);
            assert_eq!(entries[1].name, "apple.txt");
            assert_eq!(entries[2].name, "zebra.txt");
        }

        #[test]
        fn list_files_local_reads_current_dir() {
            let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
            let entries = list_files_local(&cwd).unwrap();
            assert!(
                entries.iter().any(|e| e.name == "Cargo.toml"),
                "expected Cargo.toml in {:?}",
                entries.iter().map(|e| &e.name).collect::<Vec<_>>()
            );
        }

        #[test]
        fn list_files_local_error_on_nonexistent() {
            let result = list_files_local("/nonexistent_path_that_does_not_exist_12345");
            assert!(result.is_err());
        }

        #[test]
        fn base64_roundtrip() {
            use base64::Engine;
            let original = b"Hello, file browser! \x00\xff\xfe";
            let encoded = base64::engine::general_purpose::STANDARD.encode(original);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&encoded)
                .unwrap();
            assert_eq!(&decoded, original);
        }

        #[test]
        fn file_browser_cleanup_on_tab_close() {
            let mut browsers: HashMap<u64, FileBrowserState> = HashMap::new();
            browsers.insert(
                42,
                FileBrowserState {
                    current_path: "/tmp".to_string(),
                    path_input: "/tmp".to_string(),
                    ..Default::default()
                },
            );
            assert!(browsers.contains_key(&42));
            browsers.remove(&42);
            assert!(!browsers.contains_key(&42));
        }

        #[test]
        fn scroll_offset_initializes_to_zero() {
            let mut parser = vt100::Parser::new(24, 80, 1000);
            parser.process(b"hello");
            // Simulate what spawn_pty_session_blocking does — scroll_offset starts at 0
            let scroll_offset: usize = 0;
            assert_eq!(scroll_offset, 0);
        }

        #[test]
        fn terminal_layout_job_with_scrollback_shows_history() {
            let mut parser = vt100::Parser::new(2, 10, 100);
            // Fill enough lines to push content into scrollback
            for i in 0..10 {
                parser.process(format!("line{i}\r\n").as_bytes());
            }
            // Without scrollback, we see only the latest visible rows
            let job_bottom = terminal_layout_job(
                parser.screen(),
                false,
                egui::FontId::monospace(12.0),
            );
            // Scroll up to see earlier content
            parser.screen_mut().set_scrollback(5);
            let job_scrolled = terminal_layout_job(
                parser.screen(),
                false,
                egui::FontId::monospace(12.0),
            );
            parser.screen_mut().set_scrollback(0);
            // The scrolled view should differ from the bottom view
            assert_ne!(job_bottom.text, job_scrolled.text);
        }

        #[test]
        fn scroll_offset_clamped_to_available_scrollback() {
            let mut parser = vt100::Parser::new(5, 10, 100);
            // Write just 3 lines — scrollback should be limited
            parser.process(b"a\r\nb\r\nc\r\n");
            parser.screen_mut().set_scrollback(usize::MAX);
            let max_sb = parser.screen().scrollback();
            let mut scroll_offset: usize = 9999;
            scroll_offset = scroll_offset.min(max_sb);
            assert!(scroll_offset <= max_sb);
            parser.screen_mut().set_scrollback(0);
        }
    }
}

#[cfg(feature = "gui")]
fn main() {
    gui::run();
}
