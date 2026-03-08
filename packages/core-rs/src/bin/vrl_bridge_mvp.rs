use std::error::Error;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use vrl_osc_core::app_store::{AppConfigStore, AppProfile, KeyringSecretStore};
use vrl_osc_core::config::{
    DebugConfig, DiscoveryConfig, ForwardTarget, InAppConfig, OscListen, RelayPaths,
};
use vrl_osc_core::mapping::{Curve, Mapping};
use vrl_osc_core::service::{AppBridgeService, DefaultRelayPublisherFactory};
use vrl_osc_core::smoothing::OutputTuning;

#[derive(Parser, Debug)]
#[command(name = "vibealong-mvp")]
#[command(about = "Vibealong OSC bridge MVP CLI (no JSON workflow)")]
struct Cli {
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long, default_value = "Vibealong")]
    keyring_service: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    Run {
        profile_id: String,
    },
    StopNote,
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        creator_username: String,
        #[arg(long)]
        stream_key: String,
        #[arg(long, default_value = "https://dev.vrlewds.com")]
        website_base_url: String,
        #[arg(long, default_value = "127.0.0.1")]
        osc_host: String,
        #[arg(long, default_value_t = 9001)]
        osc_port: u16,
        #[arg(long, action = clap::ArgAction::SetTrue)]
        allow_insecure_http: bool,
        #[arg(long, action = clap::ArgAction::SetTrue)]
        allow_network_osc: bool,
        #[arg(long = "allowed-sender")]
        osc_allowed_senders: Vec<String>,
        #[arg(long = "input-address")]
        input_addresses: Vec<String>,
        #[arg(long = "forward")]
        forward_targets: Vec<String>,
    },
    List,
    Show {
        profile_id: String,
    },
    Delete {
        profile_id: String,
    },
    SetKey {
        profile_id: String,
        #[arg(long)]
        stream_key: String,
    },
}

type BridgeService =
    AppBridgeService<AppConfigStore<KeyringSecretStore>, DefaultRelayPublisherFactory>;

fn default_db_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("Vibealong")
            .join("vibealong.sqlite");
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home)
                .join("vibealong")
                .join("vibealong.sqlite");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("vibealong")
                .join("vibealong.sqlite");
        }
    }

    PathBuf::from("data/vibealong.sqlite")
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(default_db_path);
    let service = open_service(&db_path, &cli.keyring_service)?;

    match cli.command {
        Command::Profile { command } => handle_profile_commands(service, command),
        Command::Run { profile_id } => run_profile(service, &profile_id),
        Command::StopNote => {
            println!("Runtime is process-bound in this MVP CLI.");
            println!("Use Ctrl+C in the running process to stop.");
            Ok(())
        }
    }
}

fn open_service(db_path: &Path, keyring_service: &str) -> Result<BridgeService, Box<dyn Error>> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let store = AppConfigStore::open(
        db_path,
        KeyringSecretStore::new(keyring_service)
            .with_legacy_service_names(vec!["VRLewdsBridge".to_string()]),
    )?;
    Ok(AppBridgeService::new(store, DefaultRelayPublisherFactory))
}

fn parse_forward_target(raw: &str) -> Result<ForwardTarget, Box<dyn Error>> {
    let mut parts = raw.split(':');
    let host = parts.next().unwrap_or("").trim().to_string();
    let port_raw = parts.next().unwrap_or("").trim();
    if host.is_empty() || port_raw.is_empty() || parts.next().is_some() {
        return Err(format!("Invalid forward target '{raw}', expected host:port").into());
    }
    let port: u16 = port_raw.parse()?;
    if port == 0 {
        return Err(format!("Invalid forward target '{raw}', port must be > 0").into());
    }
    Ok(ForwardTarget { host, port })
}

fn default_mappings() -> Vec<Mapping> {
    vec![Mapping::new(
        "/avatar/parameters/SPS_Contact",
        1.0,
        0.02,
        false,
        Curve::Linear,
        0.0,
        1.0,
    )
    .expect("default mapping valid")]
}

fn build_profile_config(
    website_base_url: String,
    creator_username: String,
    allow_insecure_http: bool,
    osc_host: String,
    osc_port: u16,
    allow_network_osc: bool,
    osc_allowed_senders: Vec<String>,
    input_addresses: Vec<String>,
    forward_targets: Vec<String>,
) -> Result<InAppConfig, Box<dyn Error>> {
    let mappings = if input_addresses.is_empty() {
        default_mappings()
    } else {
        input_addresses
            .into_iter()
            .filter_map(|address| Mapping::new(address, 1.0, 0.02, false, Curve::Linear, 0.0, 1.0))
            .collect::<Vec<_>>()
    };
    if mappings.is_empty() {
        return Err("No valid input mappings were provided".into());
    }

    let targets = forward_targets
        .iter()
        .map(|raw| parse_forward_target(raw))
        .collect::<Result<Vec<_>, _>>()?;
    let mut normalized_allowed_senders = Vec::new();
    for (idx, raw_sender) in osc_allowed_senders.iter().enumerate() {
        let trimmed = raw_sender.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: IpAddr = trimmed.parse().map_err(|_| {
            format!(
                "Invalid --allowed-sender at position {}: '{trimmed}'",
                idx + 1
            )
        })?;
        let normalized = parsed.to_string();
        if !normalized_allowed_senders
            .iter()
            .any(|existing| existing == &normalized)
        {
            normalized_allowed_senders.push(normalized);
        }
    }
    if !allow_network_osc && !normalized_allowed_senders.is_empty() {
        return Err(
            "--allowed-sender requires --allow-network-osc (loopback mode ignores external senders)"
                .into(),
        );
    }

    Ok(InAppConfig {
        website_base_url,
        creator_username,
        allow_insecure_http,
        osc_listen: OscListen {
            host: osc_host,
            port: osc_port,
        },
        allow_network_osc,
        osc_allowed_senders: normalized_allowed_senders,
        relay: RelayPaths {
            session_path: "/api/sps/session".to_string(),
            ingest_path: "/api/sps/ingest".to_string(),
        },
        debug: DebugConfig {
            log_osc: false,
            log_unmapped_only: false,
            log_configured_only: false,
            log_relay: false,
        },
        discovery: DiscoveryConfig {
            enabled: true,
            file_path: "data/osc-discovered.txt".into(),
            include_arg_types: false,
        },
        forward_targets: targets,
        mappings,
        output: OutputTuning::default(),
    })
}

fn handle_profile_commands(
    service: BridgeService,
    command: ProfileCommand,
) -> Result<(), Box<dyn Error>> {
    match command {
        ProfileCommand::Create {
            name,
            creator_username,
            stream_key,
            website_base_url,
            osc_host,
            osc_port,
            allow_insecure_http,
            allow_network_osc,
            osc_allowed_senders,
            input_addresses,
            forward_targets,
        } => {
            let config = build_profile_config(
                website_base_url,
                creator_username,
                allow_insecure_http,
                osc_host,
                osc_port,
                allow_network_osc,
                osc_allowed_senders,
                input_addresses,
                forward_targets,
            )?;
            let profile = AppProfile {
                id: String::new(),
                name,
                config,
            };
            let profile_id = service.upsert_profile(&profile, &stream_key)?;
            println!("Profile saved: {profile_id}");
            Ok(())
        }
        ProfileCommand::List => {
            let profiles = service.list_profiles()?;
            if profiles.is_empty() {
                println!("No profiles found.");
                return Ok(());
            }
            for profile in profiles {
                println!(
                    "{} | {} | creator={} | credential={} | updated={}",
                    profile.id,
                    profile.name,
                    profile.creator_username,
                    if profile.has_stream_key {
                        "set"
                    } else {
                        "missing"
                    },
                    profile.updated_at_ms
                );
            }
            Ok(())
        }
        ProfileCommand::Show { profile_id } => {
            let loaded = service.load_profile(&profile_id)?;
            let Some(loaded) = loaded else {
                println!("Profile not found: {profile_id}");
                return Ok(());
            };
            println!("id={}", loaded.profile.id);
            println!("name={}", loaded.profile.name);
            println!("creator={}", loaded.profile.config.creator_username);
            println!("website={}", loaded.profile.config.website_base_url);
            println!(
                "osc={}:{}",
                loaded.profile.config.osc_listen.host, loaded.profile.config.osc_listen.port
            );
            println!(
                "allowNetworkOsc={}",
                loaded.profile.config.allow_network_osc
            );
            println!(
                "allowedSenders={}",
                loaded.profile.config.osc_allowed_senders.len()
            );
            println!("mappings={}", loaded.profile.config.mappings.len());
            println!(
                "forwardTargets={}",
                loaded.profile.config.forward_targets.len()
            );
            println!(
                "credential={}",
                if loaded.stream_key.is_some() {
                    "set"
                } else {
                    "missing"
                }
            );
            Ok(())
        }
        ProfileCommand::Delete { profile_id } => {
            service.delete_profile(&profile_id)?;
            println!("Profile deleted: {profile_id}");
            Ok(())
        }
        ProfileCommand::SetKey {
            profile_id,
            stream_key,
        } => {
            service.set_stream_key(&profile_id, &stream_key)?;
            println!("Stream key updated for profile: {profile_id}");
            Ok(())
        }
    }
}

fn run_profile(service: BridgeService, profile_id: &str) -> Result<(), Box<dyn Error>> {
    service.start_profile(profile_id)?;
    println!("Runtime started for profile: {profile_id}");
    println!("Press Ctrl+C to stop.");

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    ctrlc::set_handler(move || {
        flag.store(false, Ordering::SeqCst);
    })?;

    while running.load(Ordering::SeqCst) {
        if let Some(snapshot) = service.runtime_snapshot()? {
            println!(
                "running={} seq={} intensity={:.3} target={:.3} relayConnected={} lastError={}",
                snapshot.running,
                snapshot.seq,
                snapshot.current_intensity,
                snapshot.target_intensity,
                snapshot.relay_connected,
                if snapshot.last_error.is_empty() {
                    "-"
                } else {
                    snapshot.last_error.as_str()
                }
            );
        }
        thread::sleep(Duration::from_secs(1));
    }

    service.stop_runtime()?;
    println!("Runtime stopped.");
    Ok(())
}
