mod commands;
#[cfg(unix)]
mod remote_agent;
#[cfg(test)]
mod tests;

use clap::Parser;
use zellij_utils::{
    cli::{CliAction, CliArgs, Command, RemoteAgentCommand, Sessions},
    consts::{create_config_and_cache_folders, VERSION},
    data::UnblockCondition,
    envs,
    input::config::Config,
    logging::*,
    setup::Setup,
    shared::web_server_base_url_from_config,
};

fn main() {
    if std::env::var_os(envs::EXECUTABLE_ENV_KEY).is_none() {
        if let Ok(current_executable) = std::env::current_exe() {
            let current_executable = current_executable.to_string_lossy().into_owned();
            envs::set_executable(current_executable);
        }
    }
    let opts = CliArgs::parse();

    if let Some(Command::RemoteAgent(command)) = opts.command.clone() {
        #[cfg(unix)]
        {
            // A remote-pty subprocess owns a terminal pane. Human-readable
            // bridge diagnostics must travel through its health sidecar/UI,
            // never stderr, which is the same PTY stream as the remote app.
            let is_terminal_bridge = matches!(&command, RemoteAgentCommand::RemotePty { .. });
            let result = match command {
                RemoteAgentCommand::Serve { socket, foreground } => {
                    remote_agent::serve(socket, foreground)
                },
                RemoteAgentCommand::Connect { socket } => remote_agent::connect(socket),
                RemoteAgentCommand::RemotePty {
                    provider,
                    workspace,
                    destination,
                    ssh_arg,
                    workspace_folder,
                    pane_id,
                    cwd,
                } => remote_transport(&provider, workspace, destination, ssh_arg, workspace_folder)
                    .and_then(|transport| {
                        remote_agent::remote_pty(transport, pane_id.as_deref(), cwd)
                    }),
                RemoteAgentCommand::RemoteClose {
                    provider,
                    workspace,
                    destination,
                    ssh_arg,
                    workspace_folder,
                    pane_id,
                } => remote_transport(&provider, workspace, destination, ssh_arg, workspace_folder)
                    .and_then(|transport| remote_agent::remote_close(transport, &pane_id)),
                RemoteAgentCommand::RemoteUpgrade {
                    provider,
                    workspace,
                    destination,
                    ssh_arg,
                    workspace_folder,
                    force,
                } => remote_transport(&provider, workspace, destination, ssh_arg, workspace_folder)
                    .and_then(|transport| {
                        remote_agent::remote_upgrade(
                            transport,
                            &zellij_utils::remote_bootstrap::reinstall_script(),
                            force,
                        )
                    }),
                RemoteAgentCommand::ForceRestart { socket } => remote_agent::force_restart(socket),
                RemoteAgentCommand::ReportState {
                    pane_id,
                    state,
                    agent,
                    agent_pid,
                } => remote_agent::report_state(&pane_id, &state, &agent, agent_pid, None),
            };
            if let Err(error) = result {
                if !is_terminal_bridge {
                    eprintln!("flock remote-agent: {error:#}");
                }
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            eprintln!("flock remote-agent is only supported on Linux");
            std::process::exit(65);
        }
    }

    configure_logger();
    create_config_and_cache_folders();
    if let Err(error) = zellij_utils::remote_session_cleanup::recover_pending_remote_closes() {
        log::warn!("failed to recover pending remote pane closes: {error}");
    }

    {
        let config = Config::try_from(&opts).ok();
        if let Some(Command::Action(cli_action)) = opts.command {
            commands::send_action_to_session(*cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Subscribe(subscribe_cli)) = opts.command {
            commands::subscribe_to_session(subscribe_cli, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Run {
            command,
            direction,
            cwd,
            floating,
            in_place,
            close_replaced_pane,
            name,
            close_on_exit,
            start_suspended,
            x,
            y,
            width,
            height,
            pinned,
            stacked,
            blocking,
            block_until_exit_success,
            block_until_exit_failure,
            block_until_exit,
            near_current_pane,
            borderless,
            tab_id,
        })) = opts.command
        {
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            let skip_plugin_cache = false; // N/A for this action

            // Compute the unblock condition
            let unblock_condition = if block_until_exit_success {
                Some(UnblockCondition::OnExitSuccess)
            } else if block_until_exit_failure {
                Some(UnblockCondition::OnExitFailure)
            } else if block_until_exit {
                Some(UnblockCondition::OnAnyExit)
            } else {
                None
            };

            let command_cli_action = CliAction::NewPane {
                command,
                plugin: None,
                direction,
                cwd,
                floating,
                in_place,
                close_replaced_pane,
                name,
                close_on_exit,
                start_suspended,
                configuration: None,
                skip_plugin_cache,
                x,
                y,
                width,
                height,
                pinned,
                stacked,
                blocking,
                block_until_exit_success: false,
                block_until_exit_failure: false,
                block_until_exit: false,
                unblock_condition,
                near_current_pane,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Plugin {
            url,
            floating,
            in_place,
            close_replaced_pane,
            configuration,
            skip_plugin_cache,
            x,
            y,
            width,
            height,
            pinned,
            borderless,
            tab_id,
        })) = opts.command
        {
            let cwd = None;
            let stacked = false;
            let blocking = false;
            let unblock_condition = None;
            let command_cli_action = CliAction::NewPane {
                command: vec![],
                plugin: Some(url),
                direction: None,
                cwd,
                floating,
                in_place,
                close_replaced_pane,
                name: None,
                close_on_exit: false,
                start_suspended: false,
                configuration,
                skip_plugin_cache,
                x,
                y,
                width,
                height,
                pinned,
                stacked,
                blocking,
                block_until_exit_success: false,
                block_until_exit_failure: false,
                block_until_exit: false,
                unblock_condition,
                near_current_pane: false,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Edit {
            file,
            direction,
            line_number,
            floating,
            in_place,
            close_replaced_pane,
            cwd,
            x,
            y,
            width,
            height,
            pinned,
            near_current_pane,
            borderless,
            tab_id,
        })) = opts.command
        {
            let mut file = file;
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            if file.is_relative() {
                if let Some(cwd) = cwd.as_ref() {
                    file = cwd.join(file);
                }
            }
            let command_cli_action = CliAction::Edit {
                file,
                direction,
                line_number,
                floating,
                in_place,
                close_replaced_pane,
                cwd,
                x,
                y,
                width,
                height,
                pinned,
                near_current_pane,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::ConvertConfig { old_config_file })) = opts.command {
            commands::convert_old_config_file(old_config_file);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::ConvertLayout { old_layout_file })) = opts.command {
            commands::convert_old_layout_file(old_layout_file);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::ConvertTheme { old_theme_file })) = opts.command {
            commands::convert_old_theme_file(old_theme_file);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Pipe {
            name,
            payload,
            args,
            plugin,
            plugin_configuration,
        })) = opts.command
        {
            let command_cli_action = CliAction::Pipe {
                name,
                payload,
                args,
                plugin,
                plugin_configuration,

                force_launch_plugin: false,
                skip_plugin_cache: false,
                floating_plugin: None,
                in_place_plugin: None,
                plugin_cwd: None,
                plugin_title: None,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
    }

    if let Some(Command::Sessions(Sessions::ListSessions {
        no_formatting,
        short,
        reverse,
    })) = opts.command
    {
        commands::list_sessions(no_formatting, short, reverse);
    } else if let Some(Command::Sessions(Sessions::ListAliases)) = opts.command {
        commands::list_aliases(opts);
    } else if let Some(Command::Sessions(Sessions::Watch { ref session_name })) = opts.command {
        commands::watch_session(session_name.clone(), opts);
    } else if let Some(Command::Sessions(Sessions::KillAllSessions { yes })) = opts.command {
        commands::kill_all_sessions(yes);
    } else if let Some(Command::Sessions(Sessions::KillSession { ref target_session })) =
        opts.command
    {
        commands::kill_session(target_session);
    } else if let Some(Command::Sessions(Sessions::DeleteAllSessions { yes, force })) = opts.command
    {
        commands::delete_all_sessions(yes, force);
    } else if let Some(Command::Sessions(Sessions::DeleteSession {
        ref target_session,
        force,
    })) = opts.command
    {
        commands::delete_session(target_session, force);
    } else if let Some(path) = opts.server {
        commands::start_server(path, opts.debug);
    } else if opts.layout.is_some() || opts.layout_string.is_some() {
        if let Some(session_name) = opts
            .session
            .as_ref()
            .cloned()
            .or_else(|| envs::get_session_name().ok())
        {
            let config = Config::try_from(&opts).ok();
            let options = Setup::from_cli_args(&opts).ok().map(|r| r.2);
            let new_layout_cli_action = CliAction::NewTab {
                layout: opts.layout.clone(),
                layout_string: opts.layout_string.clone(),
                layout_dir: options.as_ref().and_then(|o| o.layout_dir.clone()),
                name: None,
                cwd: options.as_ref().and_then(|o| o.default_cwd.clone()),
                initial_command: vec![],
                initial_plugin: None,
                close_on_exit: Default::default(),
                start_suspended: Default::default(),
                block_until_exit_success: false,
                block_until_exit_failure: false,
                block_until_exit: false,
            };
            commands::send_action_to_session(new_layout_cli_action, Some(session_name), config);
        } else {
            commands::start_client(opts);
        }
    } else if let Some(layout_for_new_session) = &opts.new_session_with_layout {
        let mut opts = opts.clone();
        opts.new_session_with_layout = None;
        opts.layout = Some(layout_for_new_session.clone());
        commands::start_client(opts);
    } else if let Some(Command::Web(web_opts)) = &opts.command {
        if web_opts.get_start() {
            let daemonize = web_opts.daemonize;
            commands::start_web_server(
                opts.clone(),
                daemonize,
                web_opts.ip,
                web_opts.port,
                web_opts.cert.clone(),
                web_opts.key.clone(),
                web_opts.server_startup_timeout,
            );
        } else if web_opts.stop {
            match commands::stop_web_server() {
                Ok(()) => {
                    println!("Stopped web server.");
                },
                Err(e) => {
                    eprintln!("Failed to stop web server: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.status {
            let mut config_options = commands::get_config_options_from_cli_args(&opts)
                .expect("Can't find config options");
            if let Some(ip) = web_opts.ip {
                config_options.web_server_ip = Some(ip);
            }
            if let Some(port) = web_opts.port {
                config_options.web_server_port = Some(port);
            }
            let web_server_base_url = web_server_base_url_from_config(config_options);
            match commands::web_server_status(&web_server_base_url, web_opts.timeout) {
                Ok(version) => {
                    let version = version.trim();
                    println!(
                        "Web server online with version: {}. Checked: {}",
                        version, web_server_base_url
                    );
                    if version != VERSION {
                        println!("");
                        println!(
                            "Note: this version differs from the current Flock version: {}.",
                            VERSION
                        );
                        println!("Consider stopping the server with: flock web --stop");
                        println!("And then restarting it with: flock web --start");
                    }
                },
                Err(_e) => {
                    println!("Web server is offline, checked: {}", web_server_base_url);
                },
            }
        } else if web_opts.create_token {
            let read_only = false;
            match commands::create_auth_token(web_opts.token_name.clone(), read_only) {
                Ok(token_and_name) => {
                    println!("Created token successfully");
                    println!("");
                    println!("{}", token_and_name);
                },
                Err(e) => {
                    eprintln!("Failed to create token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.create_read_only_token {
            let read_only = true;
            match commands::create_auth_token(web_opts.token_name.clone(), read_only) {
                Ok(token_and_name) => {
                    println!("Created token successfully");
                    println!("");
                    println!("{}", token_and_name);
                },
                Err(e) => {
                    eprintln!("Failed to create token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if let Some(token_name_to_revoke) = &web_opts.revoke_token {
            match commands::revoke_auth_token(token_name_to_revoke) {
                Ok(revoked) => {
                    if revoked {
                        println!("Successfully revoked token.");
                    } else {
                        eprintln!("Token by that name does not exist.");
                        std::process::exit(2)
                    }
                },
                Err(e) => {
                    eprintln!("Failed to revoke token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.revoke_all_tokens {
            match commands::revoke_all_auth_tokens() {
                Ok(_) => {
                    println!("Successfully revoked all auth tokens");
                },
                Err(e) => {
                    eprintln!("Failed to revoke all auth tokens: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.list_tokens {
            match commands::list_auth_tokens() {
                Ok(token_list) => {
                    for item in token_list {
                        println!("{}", item);
                    }
                },
                Err(e) => {
                    eprintln!("Failed to list tokens: {}", e);
                    std::process::exit(2)
                },
            }
        }
    } else {
        commands::start_client(opts);
    }
}

/// Build the provider transport from `remote-pty`/`remote-close` flags,
/// enforcing the per-provider required arguments clap cannot express here.
#[cfg(unix)]
fn remote_transport(
    provider: &str,
    workspace: Option<String>,
    destination: Option<String>,
    ssh_args: Vec<String>,
    workspace_folder: Option<String>,
) -> anyhow::Result<remote_agent::RemoteTransport> {
    use anyhow::{anyhow, Context};
    match provider {
        "coder" => Ok(remote_agent::RemoteTransport::Coder {
            workspace: workspace.context("--provider coder requires --workspace")?,
        }),
        "ssh" => Ok(remote_agent::RemoteTransport::Ssh {
            destination: destination.context("--provider ssh requires --destination")?,
            ssh_args,
        }),
        "devcontainer" => Ok(remote_agent::RemoteTransport::Devcontainer {
            workspace_folder: workspace_folder
                .context("--provider devcontainer requires --workspace-folder")?,
        }),
        other => Err(anyhow!(
            "unknown remote provider {other:?}; expected coder, ssh, or devcontainer"
        )),
    }
}
