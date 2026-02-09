use std::io::{self, Write};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ec2_manager::aws_context::build_context;
use ec2_manager::config::AppConfig;
use ec2_manager::diagnostics::{run_diagnostics, CheckStatus, DiagnosticsReport};
use ec2_manager::error::{AppError, Result};
use ec2_manager::filter::{apply_filters, Filters};
use ec2_manager::inventory::load_inventory;
use ec2_manager::models::{
    AuthStatus, AwsContext, DependencyStatus, Instance, Inventory, Mode, RecentConnection,
    SavedFilter, TerminalOption,
};
use ec2_manager::profile_choice;
use ec2_manager::terminal;
use ec2_manager::terminal::{
    build_launch_plan, build_ssm_port_forward_command, build_ssm_session_command,
    dependency_status, discover_terminals, launch,
};
use ec2_manager::util::{split_csv, truncate};
use ec2_manager::workflow::{find_instance, select_terminal};

#[derive(Debug, Default)]
struct CliOptions {
    mode: Option<Mode>,
    region: Option<String>,
    refresh: bool,
    include_terms: Vec<String>,
    exclude_terms: Vec<String>,
    states: Vec<String>,
    only_ssm: bool,
    connect_instance_id: Option<String>,
    terminal_id: Option<String>,
    dry_run: bool,
    list_terminals: bool,
    watch_profile: bool,
    test_launch: bool,
    favorite_instance_id: Option<String>,
    list_favorites: bool,
    list_recents: bool,
    diagnostics: bool,
    port_forward_instance_id: Option<String>,
    local_port: Option<u16>,
    remote_port: Option<u16>,
    save_filter_name: Option<String>,
    apply_saved_filter: Option<String>,
    interactive: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut options = parse_args(std::env::args().skip(1))?;

    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    let mut config = AppConfig::load()?;
    let mode = options
        .mode
        .clone()
        .unwrap_or_else(|| config.default_mode.clone());

    let context = build_context(mode.clone(), &config, options.region.as_deref())?;

    if let Some(account_id) = &context.account_id {
        if options.region.is_some() {
            config.upsert_account_region(account_id, &context.region);
            config.save()?;
        }
    }

    print_context(&context);

    let deps = dependency_status();
    print_dependencies(&deps);

    let terminals = discover_terminals();
    print_terminals(&terminals);

    if options.list_terminals {
        return Ok(());
    }

    let selected_terminal = select_terminal(&terminals, &config, options.terminal_id.as_deref())?;

    if let Some(id) = &options.terminal_id {
        config.default_terminal = Some(id.to_string());
        config.save()?;
    }

    if options.watch_profile {
        watch_profile(mode, &config)?;
        return Ok(());
    }

    if options.interactive {
        return run_interactive_shell(
            &context,
            &mut config,
            &deps,
            &terminals,
            selected_terminal.clone(),
            options.dry_run,
        );
    }

    let account_scope = context
        .account_id
        .clone()
        .unwrap_or_else(|| "unknown-account".to_string());

    if let Some(saved_name) = &options.apply_saved_filter {
        if let Some(saved) = config
            .saved_filters_for_scope(&account_scope, &context.region)
            .into_iter()
            .find(|item| item.name.eq_ignore_ascii_case(saved_name))
        {
            options.include_terms = saved.include_terms.clone();
            options.exclude_terms = saved.exclude_terms.clone();
            options.states = saved.states.clone();
            options.only_ssm = saved.only_ssm_managed;
        } else {
            return Err(AppError::NotFound(format!(
                "Saved filter not found for scope {}:{} -> {}",
                account_scope, context.region, saved_name
            )));
        }
    }

    if options.list_favorites {
        let favorites = config.favorites_for_scope(&account_scope, &context.region);
        print_favorites(&account_scope, &context.region, &favorites);
    }

    if options.list_recents {
        let recents = config.recents_for_scope(&account_scope, &context.region);
        print_recents(&account_scope, &context.region, &recents);
    }

    let inventory = if mode == Mode::Live && context.auth_status != AuthStatus::Ok {
        println!("\nAuth is not OK; inventory fetch skipped. Refresh credentials and retry.");
        Inventory {
            instances: Vec::new(),
            fetched_at: SystemTime::now(),
        }
    } else {
        load_inventory(&context, &config.tag_mapping, options.refresh)?
    };

    if let Some(instance_id) = &options.favorite_instance_id {
        let is_now_favorite = config.toggle_favorite(&account_scope, &context.region, instance_id);
        config.save()?;
        println!(
            "Favorite {} for {}:{} -> {}",
            if is_now_favorite {
                "enabled"
            } else {
                "disabled"
            },
            account_scope,
            context.region,
            instance_id
        );
    }

    if let Some(name) = &options.save_filter_name {
        config.upsert_saved_filter(
            &account_scope,
            &context.region,
            SavedFilter {
                name: name.clone(),
                include_terms: options.include_terms.clone(),
                exclude_terms: options.exclude_terms.clone(),
                states: options.states.clone(),
                only_ssm_managed: options.only_ssm,
            },
        );
        config.save()?;
        println!(
            "Saved filter '{}' for scope {}:{}",
            name, account_scope, context.region
        );
    }

    if options.diagnostics {
        let report = run_diagnostics(&context, &deps, &terminals, &config);
        print_diagnostics(&report);
    }

    if options.list_favorites || options.list_recents {
        println!();
    }

    let filters = Filters {
        includes: options.include_terms.clone(),
        excludes: options.exclude_terms.clone(),
        states: options.states.clone(),
        only_ssm_managed: options.only_ssm,
    };
    let filtered = apply_filters(&inventory.instances, &filters);

    println!(
        "\nInventory: {} instance(s) after filters (fetched at {})",
        filtered.len(),
        unix_time(inventory.fetched_at)
    );
    print_instances(&filtered, 75);

    if options.test_launch {
        let terminal = selected_terminal.clone().ok_or_else(|| {
            AppError::NotFound("No terminal available for test launch".to_string())
        })?;

        let test_cmd = if mode == Mode::Sim {
            "echo '[SIM MODE] terminal launch test'".to_string()
        } else {
            "aws --version".to_string()
        };

        let plan = build_launch_plan(
            &terminal,
            &test_cmd,
            &context.profile,
            &context.region,
            Some("ec2-manager-test"),
            true,
        );
        print_launch_plan(&plan);
        launch(&plan, options.dry_run)?;
    }

    if let Some(instance_id) = options.connect_instance_id {
        let terminal = selected_terminal.clone().ok_or_else(|| {
            AppError::NotFound(
                "No terminal found. Install/detect a supported terminal or set --terminal"
                    .to_string(),
            )
        })?;

        let instance = find_instance(&filtered, &instance_id)
            .or_else(|| find_instance(&inventory.instances, &instance_id))
            .ok_or_else(|| AppError::NotFound(format!("Instance {instance_id} not found")))?;

        if mode == Mode::Live && (!deps.aws_cli_found || !deps.ssm_plugin_found) {
            return Err(AppError::Parse(
                "Connect requires both aws CLI and session-manager-plugin in PATH".to_string(),
            ));
        }

        let base_cmd = build_ssm_session_command(&instance.instance_id, &context.region);
        let session_cmd = if mode == Mode::Sim {
            format!("echo '[SIM MODE] {base_cmd}'")
        } else {
            base_cmd
        };

        let tab_title = instance
            .name
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(&instance.instance_id);
        let plan = build_launch_plan(
            &terminal,
            &session_cmd,
            &context.profile,
            &context.region,
            Some(tab_title),
            true,
        );
        print_launch_plan(&plan);
        launch(&plan, options.dry_run)?;
        if !options.dry_run {
            config.add_recent_connection(RecentConnection {
                account_id: account_scope.clone(),
                region: context.region.clone(),
                instance_id: instance.instance_id.clone(),
                name: instance.name.clone(),
                timestamp_unix: unix_time(SystemTime::now()),
            });
            config.save()?;
        }
    }

    if let Some(instance_id) = options.port_forward_instance_id {
        let local_port = options.local_port.unwrap_or(2222);
        let remote_port = options.remote_port.unwrap_or(22);
        let terminal = selected_terminal.clone().ok_or_else(|| {
            AppError::NotFound(
                "No terminal found. Install/detect a supported terminal or set --terminal"
                    .to_string(),
            )
        })?;

        let instance = find_instance(&filtered, &instance_id)
            .or_else(|| find_instance(&inventory.instances, &instance_id))
            .ok_or_else(|| AppError::NotFound(format!("Instance {instance_id} not found")))?;

        if mode == Mode::Live && (!deps.aws_cli_found || !deps.ssm_plugin_found) {
            return Err(AppError::Parse(
                "Port forwarding requires both aws CLI and session-manager-plugin in PATH"
                    .to_string(),
            ));
        }

        let base_cmd = build_ssm_port_forward_command(
            &instance.instance_id,
            &context.region,
            local_port,
            remote_port,
        );
        let command = if mode == Mode::Sim {
            format!("echo '[SIM MODE] {base_cmd}'")
        } else {
            base_cmd
        };

        let tab_title = instance
            .name
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or(&instance.instance_id);
        let plan = build_launch_plan(
            &terminal,
            &command,
            &context.profile,
            &context.region,
            Some(tab_title),
            true,
        );
        print_launch_plan(&plan);
        launch(&plan, options.dry_run)?;
        if !options.dry_run {
            config.add_recent_connection(RecentConnection {
                account_id: account_scope.clone(),
                region: context.region.clone(),
                instance_id: instance.instance_id.clone(),
                name: instance.name.clone(),
                timestamp_unix: unix_time(SystemTime::now()),
            });
            config.save()?;
        }
    }

    Ok(())
}

fn parse_args<I>(args: I) -> Result<CliOptions>
where
    I: Iterator<Item = String>,
{
    let mut out = CliOptions::default();
    let mut iter = args.peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mode" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--mode requires a value: live|sim".to_string())
                })?;

                out.mode = Mode::parse(&value);
                if out.mode.is_none() {
                    return Err(AppError::InvalidArgument(format!(
                        "Unsupported --mode value: {value}"
                    )));
                }
            }
            "--region" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--region requires a value".to_string())
                })?;
                out.region = Some(value);
            }
            "--refresh" => out.refresh = true,
            "--search" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--search requires a value".to_string())
                })?;
                out.include_terms.push(value);
            }
            "--include" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--include requires a value".to_string())
                })?;
                out.include_terms.extend(split_csv(&value));
            }
            "--exclude" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--exclude requires a value".to_string())
                })?;
                out.exclude_terms.extend(split_csv(&value));
            }
            "--state" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--state requires a value".to_string())
                })?;
                out.states.extend(split_csv(&value));
            }
            "--only-ssm" => out.only_ssm = true,
            "--connect" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--connect requires an instance id".to_string())
                })?;
                out.connect_instance_id = Some(value);
            }
            "--terminal" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--terminal requires an id".to_string())
                })?;
                out.terminal_id = Some(value);
            }
            "--dry-run" => out.dry_run = true,
            "--list-terminals" => out.list_terminals = true,
            "--watch-profile" => out.watch_profile = true,
            "--test-launch" => out.test_launch = true,
            "--favorite" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--favorite requires an instance id".to_string())
                })?;
                out.favorite_instance_id = Some(value);
            }
            "--list-favorites" => out.list_favorites = true,
            "--list-recents" => out.list_recents = true,
            "--diagnostics" => out.diagnostics = true,
            "--port-forward" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--port-forward requires an instance id".to_string())
                })?;
                out.port_forward_instance_id = Some(value);
            }
            "--local-port" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--local-port requires a numeric value".to_string())
                })?;
                out.local_port = Some(value.parse::<u16>().map_err(|_| {
                    AppError::InvalidArgument("--local-port must be in range 1-65535".to_string())
                })?);
            }
            "--remote-port" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--remote-port requires a numeric value".to_string())
                })?;
                out.remote_port = Some(value.parse::<u16>().map_err(|_| {
                    AppError::InvalidArgument("--remote-port must be in range 1-65535".to_string())
                })?);
            }
            "--save-filter" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--save-filter requires a name".to_string())
                })?;
                out.save_filter_name = Some(value);
            }
            "--apply-filter" => {
                let value = iter.next().ok_or_else(|| {
                    AppError::InvalidArgument("--apply-filter requires a saved name".to_string())
                })?;
                out.apply_saved_filter = Some(value);
            }
            "--interactive" => out.interactive = true,
            "--help" | "-h" => {}
            unknown => {
                return Err(AppError::InvalidArgument(format!(
                    "Unknown argument: {unknown}. Use --help"
                )))
            }
        }
    }

    Ok(out)
}

fn print_help() {
    println!(
        "ec2_manager\n\
         \n\
         Usage:\n\
           ec2_manager [options]\n\
         \n\
         Options:\n\
           --mode live|sim         Runtime mode (default from config, initially live)\n\
           --region <region>       Override region for this run\n\
           --refresh               Force refresh AWS/sim inventory (bypass cache)\n\
           --search <term>         Include filter term (alias of --include)\n\
           --include <csv>         Add include term(s), all includes must match\n\
           --exclude <csv>         Add exclude term(s), excluded terms are removed\n\
           --state <csv>           Filter states, e.g. running,stopped\n\
           --only-ssm              Show only SSM-managed instances\n\
           --connect <instance>    Launch terminal and connect to an instance\n\
           --port-forward <id>     Launch terminal with SSM port forwarding session\n\
           --local-port <port>     Local port for --port-forward (default 2222)\n\
           --remote-port <port>    Remote port for --port-forward (default 22)\n\
           --terminal <id>         Force terminal id (and persist as default)\n\
           --test-launch           Launch selected terminal with a test command\n\
           --favorite <instance>   Toggle favorite for selected account+region scope\n\
           --list-favorites        Show favorites for selected account+region scope\n\
           --list-recents          Show recent connections for selected scope\n\
           --save-filter <name>    Save current include/exclude/state/only-ssm filter\n\
           --apply-filter <name>   Apply a saved filter by name before listing\n\
           --diagnostics           Run dependency and permission diagnostics\n\
           --interactive           Start interactive Rust shell mode\n\
           --dry-run               Print launch plan but do not spawn terminal\n\
           --list-terminals        Print detected terminals then exit\n\
           --watch-profile         Poll ~/.aws/profileChoice and print changes\n\
           -h, --help              Show help\n"
    );
}

fn print_context(context: &AwsContext) {
    println!("Mode:          {}", context.mode.as_str());
    println!("AWS Profile:   {}", context.profile);
    println!("Auth Status:   {}", context.auth_status);
    println!(
        "Account:       {}",
        context.account_id.as_deref().unwrap_or("(unknown)")
    );
    println!("Region:        {}", context.region);
    if let Some(arn) = &context.arn {
        println!("ARN:           {arn}");
    }
    if let Some(user_id) = &context.user_id {
        println!("User ID:       {user_id}");
    }
}

fn print_dependencies(status: &DependencyStatus) {
    println!("aws CLI:       {}", yes_no(status.aws_cli_found));
    println!("ssm plugin:    {}", yes_no(status.ssm_plugin_found));
}

fn print_terminals(terminals: &[TerminalOption]) {
    println!("Detected terminals: {}", terminals.len());
    for t in terminals {
        println!("  - {} ({}) -> {}", t.id, t.display_name, t.program);
    }
}

fn print_instances(instances: &[Instance], max_rows: usize) {
    println!(
        "{:<14} {:<20} {:<10} {:<5} {:<15} {:<8} {:<12}",
        "InstanceId", "Name", "State", "SSM", "PrivateIP", "Env", "App/Service"
    );
    println!("{}", "-".repeat(92));

    for instance in instances.iter().take(max_rows) {
        let ssm = if instance.ssm_managed {
            instance
                .ssm_ping
                .as_deref()
                .unwrap_or("Managed")
                .to_string()
        } else {
            "No".to_string()
        };

        println!(
            "{:<14} {:<20} {:<10} {:<5} {:<15} {:<8} {:<12}",
            truncate(&instance.instance_id, 14),
            truncate(instance.name.as_deref().unwrap_or(""), 20),
            truncate(&instance.state, 10),
            truncate(&ssm, 5),
            truncate(instance.private_ip.as_deref().unwrap_or(""), 15),
            truncate(instance.env.as_deref().unwrap_or(""), 8),
            truncate(instance.app_service.as_deref().unwrap_or(""), 12),
        );
    }

    if instances.len() > max_rows {
        println!("... {} more rows", instances.len() - max_rows);
    }
}

fn print_launch_plan(plan: &terminal::LaunchPlan) {
    println!("\nLaunch Plan:");
    println!("  Program: {}", plan.program);
    println!("  Args: {}", plan.args.join(" "));
    for (k, v) in &plan.env {
        println!("  Env: {k}={v}");
    }
}

fn print_favorites(account_id: &str, region: &str, favorites: &[String]) {
    println!(
        "\nFavorites for scope {}:{} ({}):",
        account_id,
        region,
        favorites.len()
    );
    for instance_id in favorites {
        println!("  - {instance_id}");
    }
}

fn print_recents(account_id: &str, region: &str, recents: &[RecentConnection]) {
    println!(
        "\nRecent connections for scope {}:{} ({}):",
        account_id,
        region,
        recents.len()
    );
    for item in recents {
        println!(
            "  - {} {} {}",
            item.instance_id,
            item.name.clone().unwrap_or_default(),
            item.timestamp_unix
        );
    }
}

fn print_diagnostics(report: &DiagnosticsReport) {
    println!("\nDiagnostics:");
    println!("  Mode: {}", report.mode);
    println!("  Profile: {}", report.profile);
    println!(
        "  profileChoice exists: {}",
        yes_no(report.profile_choice_exists)
    );
    println!("  Auth status: {}", report.auth_status);
    println!("  aws CLI: {}", yes_no(report.aws_cli_found));
    println!("  ssm plugin: {}", yes_no(report.ssm_plugin_found));
    println!("  Terminal count: {}", report.terminal_count);
    println!(
        "  Default terminal: {}",
        report.default_terminal.as_deref().unwrap_or("(none)")
    );
    println!("  EC2 check: {}", check_status_string(&report.ec2_check));
    println!("  SSM check: {}", check_status_string(&report.ssm_check));
}

fn check_status_string(status: &CheckStatus) -> String {
    match status {
        CheckStatus::Ok => "ok".to_string(),
        CheckStatus::Skipped(reason) => format!("skipped ({reason})"),
        CheckStatus::Failed(reason) => format!("failed ({})", truncate(reason, 120)),
    }
}

fn run_interactive_shell(
    context: &AwsContext,
    config: &mut AppConfig,
    deps: &DependencyStatus,
    terminals: &[TerminalOption],
    mut selected_terminal: Option<TerminalOption>,
    dry_run: bool,
) -> Result<()> {
    let account_scope = context
        .account_id
        .clone()
        .unwrap_or_else(|| "unknown-account".to_string());

    let mut include_terms: Vec<String> = Vec::new();
    let mut exclude_terms: Vec<String> = Vec::new();
    let mut states: Vec<String> = Vec::new();
    let mut only_ssm = false;

    println!("\nInteractive mode (Rust-only). Type 'help' for commands.");

    loop {
        print!("ec2-manager> ");
        io::stdout().flush()?;

        let mut line = String::new();
        let bytes = io::stdin().read_line(&mut line)?;
        if bytes == 0 {
            println!();
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if line.eq_ignore_ascii_case("quit") || line.eq_ignore_ascii_case("exit") {
            break;
        }

        if line.eq_ignore_ascii_case("help") {
            println!("Commands:");
            println!("  list");
            println!("  refresh");
            println!("  search <term>");
            println!("  include <term>");
            println!("  exclude <term>");
            println!("  clear-includes");
            println!("  clear-excludes");
            println!("  state <csv>");
            println!("  ssm on|off");
            println!("  connect <instance-id>");
            println!("  port-forward <instance-id> [local] [remote]");
            println!("  favorite <instance-id>");
            println!("  favorites");
            println!("  recents");
            println!("  save-filter <name>");
            println!("  apply-filter <name>");
            println!("  diagnostics");
            println!("  terminal <id>");
            println!("  terminals");
            println!("  quit");
            continue;
        }

        if line.eq_ignore_ascii_case("terminals") {
            print_terminals(terminals);
            continue;
        }

        if let Some(id) = line.strip_prefix("terminal ").map(str::trim) {
            selected_terminal = select_terminal(terminals, config, Some(id))?;
            config.default_terminal = Some(id.to_string());
            config.save()?;
            println!("Default terminal set to: {id}");
            continue;
        }

        if let Some(value) = line.strip_prefix("search ").map(str::trim) {
            include_terms.clear();
            if !value.is_empty() {
                include_terms.push(value.to_string());
            }
            println!("include terms replaced");
            continue;
        }

        if let Some(value) = line.strip_prefix("include ").map(str::trim) {
            if value.is_empty() {
                println!("include term required");
            } else {
                include_terms.push(value.to_string());
                println!("include term added");
            }
            continue;
        }

        if let Some(value) = line.strip_prefix("exclude ").map(str::trim) {
            if value.is_empty() {
                println!("exclude term required");
            } else {
                exclude_terms.push(value.to_string());
                println!("exclude term added");
            }
            continue;
        }

        if line.eq_ignore_ascii_case("clear-includes") {
            include_terms.clear();
            println!("include terms cleared");
            continue;
        }

        if line.eq_ignore_ascii_case("clear-excludes") {
            exclude_terms.clear();
            println!("exclude terms cleared");
            continue;
        }

        if let Some(value) = line.strip_prefix("state ").map(str::trim) {
            states = split_csv(value);
            println!("state filter set");
            continue;
        }

        if line.eq_ignore_ascii_case("ssm on") {
            only_ssm = true;
            println!("only-ssm enabled");
            continue;
        }
        if line.eq_ignore_ascii_case("ssm off") {
            only_ssm = false;
            println!("only-ssm disabled");
            continue;
        }

        if line.eq_ignore_ascii_case("favorites") {
            print_favorites(
                &account_scope,
                &context.region,
                &config.favorites_for_scope(&account_scope, &context.region),
            );
            continue;
        }

        if line.eq_ignore_ascii_case("recents") {
            print_recents(
                &account_scope,
                &context.region,
                &config.recents_for_scope(&account_scope, &context.region),
            );
            continue;
        }

        if let Some(instance_id) = line.strip_prefix("favorite ").map(str::trim) {
            if instance_id.is_empty() {
                println!("instance id required");
                continue;
            }
            let enabled = config.toggle_favorite(&account_scope, &context.region, instance_id);
            config.save()?;
            println!(
                "favorite {}: {}",
                if enabled { "enabled" } else { "disabled" },
                instance_id
            );
            continue;
        }

        if let Some(name) = line.strip_prefix("save-filter ").map(str::trim) {
            if name.is_empty() {
                println!("filter name required");
                continue;
            }
            config.upsert_saved_filter(
                &account_scope,
                &context.region,
                SavedFilter {
                    name: name.to_string(),
                    include_terms: include_terms.clone(),
                    exclude_terms: exclude_terms.clone(),
                    states: states.clone(),
                    only_ssm_managed: only_ssm,
                },
            );
            config.save()?;
            println!("saved filter: {name}");
            continue;
        }

        if let Some(name) = line.strip_prefix("apply-filter ").map(str::trim) {
            if let Some(saved) = config
                .saved_filters_for_scope(&account_scope, &context.region)
                .into_iter()
                .find(|item| item.name.eq_ignore_ascii_case(name))
            {
                include_terms = saved.include_terms;
                exclude_terms = saved.exclude_terms;
                states = saved.states;
                only_ssm = saved.only_ssm_managed;
                println!("applied filter: {name}");
            } else {
                println!("filter not found: {name}");
            }
            continue;
        }

        if line.eq_ignore_ascii_case("diagnostics") {
            let report = run_diagnostics(context, deps, terminals, config);
            print_diagnostics(&report);
            continue;
        }

        let force_refresh = line.eq_ignore_ascii_case("refresh");
        if line.eq_ignore_ascii_case("list") || force_refresh {
            let inventory = load_inventory(context, &config.tag_mapping, force_refresh)?;
            let filtered = apply_filters(
                &inventory.instances,
                &Filters {
                    includes: include_terms.clone(),
                    excludes: exclude_terms.clone(),
                    states: states.clone(),
                    only_ssm_managed: only_ssm,
                },
            );
            print_instances(&filtered, 75);
            continue;
        }

        if let Some(instance_id) = line.strip_prefix("connect ").map(str::trim) {
            if instance_id.is_empty() {
                println!("instance id required");
                continue;
            }

            let inventory = load_inventory(context, &config.tag_mapping, false)?;
            let filtered = apply_filters(
                &inventory.instances,
                &Filters {
                    includes: include_terms.clone(),
                    excludes: exclude_terms.clone(),
                    states: states.clone(),
                    only_ssm_managed: only_ssm,
                },
            );
            let Some(instance) = find_instance(&filtered, instance_id)
                .or_else(|| find_instance(&inventory.instances, instance_id))
            else {
                println!("instance not found: {instance_id}");
                continue;
            };

            if context.mode == Mode::Live && (!deps.aws_cli_found || !deps.ssm_plugin_found) {
                println!("connect blocked: aws CLI and session-manager-plugin required");
                continue;
            }

            let Some(terminal) = selected_terminal.clone() else {
                println!("no terminal selected");
                continue;
            };

            let base_cmd = build_ssm_session_command(&instance.instance_id, &context.region);
            let command = if context.mode == Mode::Sim {
                format!("echo '[SIM MODE] {base_cmd}'")
            } else {
                base_cmd
            };

            let tab_title = instance
                .name
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(&instance.instance_id);
            let plan = build_launch_plan(
                &terminal,
                &command,
                &context.profile,
                &context.region,
                Some(tab_title),
                true,
            );
            print_launch_plan(&plan);
            launch(&plan, dry_run)?;
            if !dry_run {
                config.add_recent_connection(RecentConnection {
                    account_id: account_scope.clone(),
                    region: context.region.clone(),
                    instance_id: instance.instance_id.clone(),
                    name: instance.name.clone(),
                    timestamp_unix: unix_time(SystemTime::now()),
                });
                config.save()?;
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("port-forward ").map(str::trim) {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.is_empty() {
                println!("usage: port-forward <instance-id> [local] [remote]");
                continue;
            }
            let instance_id = parts[0];
            let local_port = parts
                .get(1)
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(2222);
            let remote_port = parts
                .get(2)
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(22);

            let inventory = load_inventory(context, &config.tag_mapping, false)?;
            let Some(instance) = find_instance(&inventory.instances, instance_id) else {
                println!("instance not found: {instance_id}");
                continue;
            };

            if context.mode == Mode::Live && (!deps.aws_cli_found || !deps.ssm_plugin_found) {
                println!("port-forward blocked: aws CLI and session-manager-plugin required");
                continue;
            }

            let Some(terminal) = selected_terminal.clone() else {
                println!("no terminal selected");
                continue;
            };

            let base_cmd = build_ssm_port_forward_command(
                &instance.instance_id,
                &context.region,
                local_port,
                remote_port,
            );
            let command = if context.mode == Mode::Sim {
                format!("echo '[SIM MODE] {base_cmd}'")
            } else {
                base_cmd
            };

            let tab_title = instance
                .name
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or(&instance.instance_id);
            let plan = build_launch_plan(
                &terminal,
                &command,
                &context.profile,
                &context.region,
                Some(tab_title),
                true,
            );
            print_launch_plan(&plan);
            launch(&plan, dry_run)?;
            if !dry_run {
                config.add_recent_connection(RecentConnection {
                    account_id: account_scope.clone(),
                    region: context.region.clone(),
                    instance_id: instance.instance_id.clone(),
                    name: instance.name.clone(),
                    timestamp_unix: unix_time(SystemTime::now()),
                });
                config.save()?;
            }
            continue;
        }

        println!("unknown command: {line}");
    }

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn unix_time(ts: SystemTime) -> u64 {
    ts.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn watch_profile(mode: Mode, config: &AppConfig) -> Result<()> {
    let path = profile_choice::profile_choice_path().ok_or_else(|| {
        AppError::NotFound("Could not resolve ~/.aws/profileChoice path".to_string())
    })?;

    println!("Watching {} for changes...", path.to_string_lossy());
    let mut last_mtime = None;

    loop {
        let metadata = std::fs::metadata(&path);
        let mtime = metadata.ok().and_then(|m| m.modified().ok());

        if mtime != last_mtime {
            last_mtime = mtime;
            let context = build_context(mode.clone(), config, None)?;
            println!("\nDetected profile change:");
            print_context(&context);
        }

        thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_cli_options() {
        let args = vec![
            "--mode".to_string(),
            "sim".to_string(),
            "--favorite".to_string(),
            "i-1".to_string(),
            "--port-forward".to_string(),
            "i-2".to_string(),
            "--local-port".to_string(),
            "15432".to_string(),
            "--remote-port".to_string(),
            "5432".to_string(),
            "--save-filter".to_string(),
            "prod".to_string(),
            "--apply-filter".to_string(),
            "prod".to_string(),
            "--search".to_string(),
            "orders".to_string(),
            "--include".to_string(),
            "platform,prod".to_string(),
            "--exclude".to_string(),
            "legacy".to_string(),
            "--diagnostics".to_string(),
            "--interactive".to_string(),
        ];

        let parsed = parse_args(args.into_iter()).expect("args should parse");
        assert_eq!(parsed.mode, Some(Mode::Sim));
        assert_eq!(parsed.favorite_instance_id.as_deref(), Some("i-1"));
        assert_eq!(parsed.port_forward_instance_id.as_deref(), Some("i-2"));
        assert_eq!(parsed.local_port, Some(15432));
        assert_eq!(parsed.remote_port, Some(5432));
        assert_eq!(parsed.save_filter_name.as_deref(), Some("prod"));
        assert_eq!(parsed.apply_saved_filter.as_deref(), Some("prod"));
        assert_eq!(
            parsed.include_terms,
            vec![
                "orders".to_string(),
                "platform".to_string(),
                "prod".to_string()
            ]
        );
        assert_eq!(parsed.exclude_terms, vec!["legacy".to_string()]);
        assert!(parsed.diagnostics);
        assert!(parsed.interactive);
    }

    #[test]
    fn invalid_port_is_rejected() {
        let args = vec!["--local-port".to_string(), "99999".to_string()];
        let err = parse_args(args.into_iter()).expect_err("port should be invalid");
        assert!(err.to_string().contains("--local-port"));
    }

    #[test]
    fn dry_run_parses_with_port_forward() {
        let args = vec![
            "--mode".to_string(),
            "sim".to_string(),
            "--port-forward".to_string(),
            "i-1".to_string(),
            "--local-port".to_string(),
            "15432".to_string(),
            "--remote-port".to_string(),
            "5432".to_string(),
            "--dry-run".to_string(),
        ];

        let parsed = parse_args(args.into_iter()).expect("args should parse");
        assert!(parsed.dry_run);
        assert_eq!(parsed.port_forward_instance_id.as_deref(), Some("i-1"));
    }
}
