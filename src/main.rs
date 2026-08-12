use anyhow::Result;
use clap::Parser;
use std::io::IsTerminal;
use std::path::Path;

use rite::cli::{self, Cli, Commands, OutputFormat};
use rite::core::project::ensure_data_dir;

fn main() -> Result<()> {
    let _telemetry = rite::telemetry::init();

    // Detect which binary name was used to invoke this program
    let _program_name = std::env::args()
        .next()
        .and_then(|arg0| {
            Path::new(&arg0)
                .file_name()
                .and_then(|name| name.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "rite".to_string());

    let cli = Cli::parse();

    // Resolve effective output format with cascade:
    // 1. --json flag (deprecated, backwards compatibility)
    // 2. --format flag explicitly set
    // 3. FORMAT env var
    // 4. TTY detection: TTY -> Pretty, non-TTY -> Text
    let format = if cli.json {
        OutputFormat::Json
    } else if let Some(fmt) = cli.format {
        // --format was explicitly set
        fmt
    } else if let Ok(env_format) = std::env::var("FORMAT") {
        // Try to parse FORMAT env var
        match env_format.to_lowercase().as_str() {
            "pretty" => OutputFormat::Pretty,
            "text" => OutputFormat::Text,
            "json" => OutputFormat::Json,
            _ => {
                // Invalid format, fall back to TTY detection
                if std::io::stdout().is_terminal() {
                    OutputFormat::Pretty
                } else {
                    OutputFormat::Text
                }
            }
        }
    } else {
        // TTY auto-detection
        if std::io::stdout().is_terminal() {
            OutputFormat::Pretty
        } else {
            OutputFormat::Text
        }
    };

    // Ensure data directory exists for most commands
    // (init creates it explicitly; generate-name, doctor, and tldr don't require it)
    if !matches!(
        cli.command,
        Commands::GenerateName | Commands::Init | Commands::Doctor | Commands::Tldr
    ) {
        ensure_data_dir()?;

        // Record a presence heartbeat for whoever is running this command.
        // This is how presence is derived (see rite::core::presence): every
        // invocation that resolves an identity counts as a liveness signal,
        // so agents never have to remember to set or clear anything.
        // Best-effort — a heartbeat write failure must not block the actual
        // command.
        if let Some(agent) = rite::core::identity::resolve_agent(cli.agent.as_deref()) {
            let _ = rite::core::presence::record_heartbeat(&agent);
        }
    }

    match cli.command {
        Commands::Init => cli::init::run(),

        Commands::Doctor => cli::doctor::run(format),

        Commands::GenerateName => {
            cli::names::run();
            Ok(())
        }

        Commands::Tldr => {
            cli::tldr::run();
            Ok(())
        }

        Commands::Whoami {
            suggest_project_suffix,
        } => cli::whoami::run(format, cli.agent.as_deref(), suggest_project_suffix),

        Commands::Send {
            target,
            message,
            meta,
            labels,
            attachments,
            reply_to,
            no_hooks,
            format: local_format,
        } => cli::send::run(
            cli::send::SendOptions {
                target,
                message,
                meta,
                labels,
                attachments,
                reply_to,
                no_hooks,
                format: if cli.json {
                    OutputFormat::Json
                } else {
                    local_format.unwrap_or(format)
                },
            },
            cli.agent.as_deref(),
        ),

        Commands::History {
            channel,
            channel_named,
            count,
            follow,
            timeout,
            follow_count,
            since,
            before,
            from,
            labels,
            after_offset,
            after_id,
            thread,
            show_offset,
            format: local_format,
        } => cli::history::run(cli::history::HistoryOptions {
            channel: channel.or(channel_named),
            count,
            follow,
            timeout,
            follow_count,
            since,
            before,
            from,
            labels,
            after_offset,
            after_id,
            thread,
            show_offset,
            // Use local format if provided, otherwise default to Text for history
            format: if cli.json {
                OutputFormat::Json
            } else {
                local_format.unwrap_or(format)
            },
            agent: cli.agent.clone(),
        }),

        Commands::Watch { channel, all } => cli::watch::run(channel, all),

        Commands::Channels { command } => {
            use cli::ChannelsCommands;
            // Default to List if no subcommand provided (backward compatibility)
            match command.as_ref().unwrap_or(&ChannelsCommands::List {
                mine: false,
                all: false,
            }) {
                ChannelsCommands::List { mine, all } => {
                    cli::channels::list(format, *mine, *all, cli.agent.as_deref())
                }
                ChannelsCommands::Close { channel } => cli::channels::close(channel),
                ChannelsCommands::Reopen { channel } => cli::channels::reopen(channel),
                ChannelsCommands::Delete { channel } => cli::channels::delete(channel),
                ChannelsCommands::Rename { old_name, new_name } => {
                    cli::channels::rename(old_name, new_name)
                }
            }
        }

        Commands::Agents { active } => cli::agents::run(format, active),

        Commands::Search {
            query,
            channel,
            count,
            from,
        } => cli::search::run(cli::search::SearchOptions {
            query,
            channel,
            count,
            from,
            format,
        }),

        Commands::Claims { command } => {
            use cli::ClaimsCommands;
            match command {
                ClaimsCommands::Stake {
                    patterns,
                    ttl,
                    message,
                } => cli::claim::claim(cli::claim::ClaimOptions {
                    patterns,
                    ttl,
                    message,
                    extend: None,
                    agent: cli.agent,
                }),
                ClaimsCommands::Refresh { patterns, ttl } => {
                    cli::claim::claim(cli::claim::ClaimOptions {
                        patterns: vec![],
                        ttl,
                        message: None,
                        extend: Some(patterns.join(" ")),
                        agent: cli.agent,
                    })
                }
                ClaimsCommands::Release { patterns, all } => {
                    cli::claim::release(patterns, all, cli.agent.as_deref())
                }
                ClaimsCommands::List {
                    all,
                    mine,
                    limit,
                    since,
                } => cli::claim::claims(format, all, mine, limit, since, cli.agent.as_deref()),
                ClaimsCommands::Check { path } => {
                    let safe = cli::claim::check_claim(path, format, cli.agent.as_deref())?;
                    if !safe {
                        std::process::exit(1);
                    }
                    Ok(())
                }
            }
        }

        Commands::Ui { channel } => cli::ui::run(channel),

        Commands::MarkRead {
            channel,
            offset,
            last_id,
        } => cli::mark_read::run(
            cli::mark_read::MarkReadOptions {
                channel,
                offset,
                last_id,
            },
            cli.agent.as_deref(),
        ),

        Commands::Inbox {
            channels,
            all,
            count,
            limit_per_channel,
            mark_read,
            mentions,
            count_only,
            format: local_format,
        } => cli::inbox::run(
            cli::inbox::InboxOptions {
                channels,
                count,
                limit_per_channel,
                mark_read,
                // Use local format if provided, otherwise default to Text for inbox
                format: if cli.json {
                    OutputFormat::Json
                } else {
                    local_format.unwrap_or(format)
                },
                all,
                mentions,
                count_only,
            },
            cli.agent.as_deref(),
        ),

        Commands::Mentions { command } => {
            use cli::MentionsCommands;
            match command {
                MentionsCommands::Follow {
                    no_dms,
                    labels,
                    timeout,
                    count,
                } => cli::mentions::follow(
                    cli::mentions::FollowOptions {
                        // DMs stream by default; --no-dms opts out.
                        include_dms: !no_dms,
                        labels,
                        timeout,
                        count,
                        format,
                    },
                    cli.agent.as_deref(),
                ),
            }
        }

        Commands::Status => cli::status::run(format, cli.agent.as_deref()),

        Commands::Wait {
            mentions,
            channels,
            labels,
            from,
            reply_to,
            allow_missing_parent,
            timeout,
        } => cli::wait::run(
            cli::wait::WaitOptions {
                mentions,
                channels,
                labels,
                from,
                reply_to,
                allow_missing_parent,
                timeout,
                format,
            },
            cli.agent.as_deref(),
        ),

        Commands::AgentsMd { command } => match command {
            cli::AgentsMdCommands::Init { file, remove } => cli::agentsmd::run_init(file, remove),
            cli::AgentsMdCommands::Show => cli::agentsmd::run_show(),
        },

        Commands::Subscriptions { command } => {
            use cli::SubscriptionsCommands;
            match command {
                SubscriptionsCommands::Add { channel } => {
                    cli::subscribe::subscribe(channel, cli.agent.as_deref())
                }
                SubscriptionsCommands::Remove { channel } => {
                    cli::subscribe::unsubscribe(channel, cli.agent.as_deref())
                }
                SubscriptionsCommands::List => {
                    cli::subscribe::list_subscriptions(cli.agent.as_deref())
                }
            }
        }

        Commands::Hooks { command } => {
            use cli::HooksCommands;
            match command {
                HooksCommands::Add {
                    channel,
                    claim,
                    mention,
                    cwd,
                    cooldown,
                    command,
                    ttl,
                    release_on_exit,
                    claim_owner,
                    priority,
                    require_flag,
                    description,
                    lease,
                    lease_ttl,
                    max_batch,
                } => cli::hooks::add(
                    channel,
                    claim,
                    mention,
                    cwd,
                    cooldown,
                    command,
                    ttl,
                    release_on_exit,
                    claim_owner,
                    priority,
                    require_flag,
                    description,
                    lease,
                    lease_ttl,
                    max_batch,
                    cli.agent.as_deref(),
                    format,
                ),
                HooksCommands::List => cli::hooks::list(format),
                HooksCommands::Set {
                    hook_id,
                    channel,
                    claim,
                    mention,
                    cwd,
                    cooldown,
                    lease,
                    no_lease,
                    lease_ttl,
                    max_batch,
                    ttl,
                    release_on_exit,
                    claim_owner,
                    priority,
                    require_flag,
                    description,
                    command,
                } => cli::hooks::set(
                    hook_id,
                    &cli::hooks::HookEdit {
                        channel,
                        claim,
                        mention,
                        cwd,
                        cooldown,
                        command,
                        ttl,
                        release_on_exit,
                        claim_owner,
                        priority,
                        require_flag,
                        description,
                        lease,
                        no_lease,
                        lease_ttl,
                        max_batch,
                    },
                    format,
                ),
                HooksCommands::Remove { hook_id } => cli::hooks::remove(hook_id, format),
                HooksCommands::Test { hook_id } => cli::hooks::test(hook_id, format),
            }
        }

        Commands::Statuses { command } => {
            use cli::StatusesCommands;
            match command {
                StatusesCommands::Set { message, ttl } => {
                    cli::statuses::set(&message, &ttl, cli.agent.as_deref(), format)
                }
                StatusesCommands::Clear => cli::statuses::clear(cli.agent.as_deref(), format),
                StatusesCommands::List => cli::statuses::list(format, cli.agent.as_deref()),
            }
        }

        Commands::Telegram => cli::telegram::run(),

        Commands::Messages { command } => {
            use cli::MessagesCommands;
            match command {
                MessagesCommands::Get { id } => cli::messages::get(&id, format),
                MessagesCommands::Delete { id, yes } => {
                    cli::messages::delete(&id, yes, cli.agent.as_deref())
                }
            }
        }

        Commands::Sync { command } => {
            use cli::SyncCommands;
            match command.as_ref() {
                Some(SyncCommands::Init { remote }) => cli::sync::init(remote.clone()),
                Some(SyncCommands::Push) => cli::sync::push(),
                Some(SyncCommands::Pull) => cli::sync::pull(),
                Some(SyncCommands::Status) => cli::sync::status(format),
                Some(SyncCommands::Log { count }) => cli::sync::log(*count, format),
                Some(SyncCommands::Check) => cli::sync::check(format),
                Some(SyncCommands::Commit { message }) => cli::sync::commit(message.clone()),
                None => {
                    // Default: push
                    cli::sync::push()
                }
            }
        }

        Commands::Index { command } => {
            use cli::IndexCommands;
            match command {
                IndexCommands::Rebuild { if_needed } => cli::index::rebuild(if_needed),
                IndexCommands::Status => cli::index::status(),
            }
        }
    }
}
