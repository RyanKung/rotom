#![deny(missing_docs)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::nursery)]

//! `rotom` command-line entrypoint.
//!
//! The binary provides login, serving, token refresh, status inspection, and
//! daemon management commands on top of the library modules exposed by
//! `rotom`.

use clap::{ArgAction, Args, Parser, Subcommand};
use reqwest::Client;
use rotom::{
    Error, Result,
    codex::client::CodexClient,
    config::{AppConfig, AppConfigStore, AuthStore, Credentials, Provider, now_unix},
    daemon::{self, DaemonInstallOptions},
    logging::{self, LogLevel},
    models::{
        highlight_model_ids_for_provider, resolve_model_ids_for_provider,
        resolve_model_list_for_providers,
    },
    oauth::{
        CodexOAuthClient, GrokOAuthClient, KiroAuthorizationCallback, KiroOAuthClient,
        create_authorization_flow, default_cli_database_path, default_desktop_token_path,
        parse_kiro_authorization_callback,
    },
    server::{AppState, UpstreamState, serve_all},
    timefmt::format_duration,
    token::TokenManager,
};
use std::{
    io::{self, IsTerminal, Write},
    net::{Ipv4Addr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    process::Command as ProcessCommand,
    time::Duration,
};
use tokio::time::{MissedTickBehavior, interval};

mod prompt;
use prompt::{
    config_string, prompt_authorization_code, prompt_optional_path, prompt_optional_string,
    prompt_port, prompt_string,
};

const INTERACTIVE_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const LOG_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_MODEL_FALLBACK: &str = "gpt-5.5";
const CLI_LONG_ABOUT: &str = "\
rotom is a local OpenAI- and Anthropic-compatible API gateway backed by Codex,
Grok, or Kiro OAuth.

It helps clients that speak either the OpenAI Chat Completions API or the
Anthropic Messages API call the selected upstream after you complete the OAuth
login flow. Credentials are stored locally and can be refreshed automatically
during requests or manually with the refresh command/API.";
const CLI_AFTER_LONG_HELP: &str = "\
Examples:
  rotom login
  rotom login --provider grok
  rotom login --kiro
  rotom config
  rotom config show
  rotom serve
  rotom serve --bind 127.0.0.1:14550 --api-key local-secret
  rotom serve --bind 127.0.0.1:14550,192.168.1.0/24:14550 --api-key local-secret
  rotom daemon install
  rotom daemon reinstall
  rotom daemon start
  rotom daemon status
  rotom models
  rotom models --provider grok
  rotom kiro import --from cli
  rotom refresh
  rotom status
  rotom update
  curl -X POST http://127.0.0.1:14550/v1/auth/refresh \\
    -H 'authorization: Bearer local-secret'

Environment:
  ROTOM_API_KEY          Optional local API key for server endpoints
  ROTOM_PROVIDER         Upstream provider: codex, grok, or kiro
  ROTOM_MODEL_FALLBACK   Fallback for unsupported Anthropic model ids
  ROTOM_AUTH_FILE        Override the credential file path
  ROTOM_HOME             Override the default config home
Files:
  Credentials default to ~/.rotom/auth.json.
  Runtime config defaults to ~/.rotom/config.json.

Disclaimer:
  rotom is an unofficial tool and is not affiliated with, endorsed by, or
  supported by OpenAI, Anthropic, xAI, AWS, or Kiro. Use it at your own
  risk, make sure your usage complies with the terms that apply to your account
  and the upstream services, and do not assume the LGPLv3 license overrides
  upstream account restrictions on sharing or reselling personal OAuth-backed
  access.

Copyright:
  Copyright (c) 2026 rotom contributors. Licensed under the GNU Lesser
  General Public License v3.0 only.";

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "rotom",
    version,
    about = "OpenAI- and Anthropic-compatible API gateway backed by OAuth providers",
    long_about = CLI_LONG_ABOUT,
    after_long_help = CLI_AFTER_LONG_HELP
)]
struct Cli {
    #[arg(
        short = 'v',
        long = "verbose",
        global = true,
        action = ArgAction::Count,
        help = "Increase logging verbosity: -v for request summaries, -vv/-vvv for full tracing"
    )]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

/// Supported top-level CLI commands.
#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Log in with an OAuth provider and save local credentials",
        long_about = "Start the selected OAuth login flow, exchange the authorization code for tokens, and save credentials to the configured auth file."
    )]
    Login {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "ROTOM_PROVIDER",
            value_name = "PROVIDER",
            help = "OAuth provider to authenticate: codex, grok, or kiro"
        )]
        provider: Option<String>,
        #[arg(
            long,
            action = ArgAction::SetTrue,
            conflicts_with = "provider",
            help = "Authenticate with Kiro without scanning local Kiro credential stores"
        )]
        kiro: bool,
        #[arg(
            long,
            default_value = "pi",
            value_name = "NAME",
            help = "OAuth originator parameter to send during login"
        )]
        originator: String,
    },
    #[command(
        about = "Manage persisted runtime configuration",
        long_about = "Interactively save or inspect default host, port, and API key stored in the rotom config file."
    )]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    #[command(
        about = "Serve the OpenAI- and Anthropic-compatible HTTP API",
        long_about = "Serve OpenAI- and Anthropic-compatible endpoints backed by the selected provider, plus Grok-native REST and WebSocket /v1/tts and /v1/tts/voices when Grok credentials are available. Other routes include /v1/models, /v1/chat/completions, /v1/responses, Responses resource compatibility routes, /v1/messages, /v1/messages/count_tokens, /v1/messages/batches, and /v1/auth/refresh."
    )]
    Serve {
        #[arg(
            long,
            value_name = "ADDR[,ADDR...]",
            help = "Socket address or comma-separated socket addresses to listen on"
        )]
        bind: Option<BindAddresses>,
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "ROTOM_API_KEY",
            value_name = "KEY",
            help = "Optional local API key accepted as Bearer token or x-api-key"
        )]
        api_key: Option<String>,
        #[arg(
            long,
            env = "ROTOM_PROVIDER",
            value_name = "PROVIDER",
            help = "Upstream provider to serve: codex, grok, or kiro"
        )]
        provider: Option<String>,
        #[arg(
            long,
            env = "ROTOM_MODEL_FALLBACK",
            value_name = "MODEL",
            help = "Fallback model for unsupported Anthropic model ids such as claude-sonnet-*"
        )]
        model_fallback: Option<String>,
    },
    #[command(
        about = "Force refresh saved OAuth tokens",
        long_about = "Use saved refresh tokens to fetch fresh credentials immediately and write them back to the configured auth file."
    )]
    Refresh {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "ROTOM_PROVIDER",
            value_name = "PROVIDER",
            help = "OAuth provider to refresh: codex, grok, or kiro. When omitted, refreshes all saved providers."
        )]
        provider: Option<String>,
    },
    #[command(
        about = "Show provider token status, highlight models, and daemon endpoints",
        long_about = "Refresh saved credentials if needed, then show token expiry, authentication status, and strongest highlight models for saved providers. When --provider is omitted, reports all saved providers. If the daemon is running, also prints its local API endpoint URLs. Use `rotom models` for full model lists."
    )]
    Status {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PROVIDER",
            help = "Provider to inspect: codex/openai, grok/xai, or kiro"
        )]
        provider: Option<String>,
    },
    #[command(
        about = "List all available models grouped by provider",
        long_about = "Print all model identifiers rotom exposes through /v1/models, grouped by upstream provider. Use --provider to list only one provider."
    )]
    Models {
        #[arg(
            long,
            value_name = "PROVIDER",
            help = "Provider to list: codex/openai, grok/xai, or kiro"
        )]
        provider: Option<String>,
    },
    #[command(
        about = "Import local Kiro credentials",
        long_about = "Import credentials from the official Kiro CLI SQLite store or Kiro IDE desktop JSON token file. This is separate from `rotom login --kiro` and never prints raw tokens."
    )]
    Kiro {
        #[command(subcommand)]
        command: KiroCommand,
    },
    #[command(
        about = "Install the latest rotom release with cargo",
        long_about = "Run `cargo install --locked --force rotom` so the currently installed rotom binary is updated to the latest published release. Requires `cargo` to be available on PATH."
    )]
    Update {
        #[arg(
            long,
            value_name = "VERSION",
            help = "Install a specific published version instead of the latest release"
        )]
        version: Option<String>,
    },
    #[command(
        about = "Install and control the background rotom service",
        long_about = "Install and control rotom as a per-user background service. macOS uses launchd LaunchAgents; Linux uses systemd user services."
    )]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

/// Background service management subcommands.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    #[command(
        about = "Install rotom as a per-user autostart service",
        long_about = "Write the service definition for the current user and enable autostart. Use `rotom daemon start` to start it immediately."
    )]
    Install(#[command(flatten)] DaemonInstallCliOptions),
    #[command(
        about = "Reinstall rotom with updated service configuration",
        long_about = "Remove the existing per-user daemon definition if present, then install a fresh one using the provided options and saved runtime config defaults."
    )]
    Reinstall(#[command(flatten)] DaemonInstallCliOptions),
    #[command(about = "Start the installed rotom daemon")]
    Start,
    #[command(about = "Restart the installed rotom daemon")]
    Restart,
    #[command(about = "Show the installed rotom daemon status")]
    Status,
    #[command(about = "Stop the installed rotom daemon")]
    Stop,
    #[command(about = "Disable and remove the installed rotom daemon")]
    Uninstall,
}

/// Runtime configuration management subcommands.
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    #[command(about = "Print the saved runtime configuration as JSON")]
    Show,
    #[command(about = "Delete the saved runtime configuration file")]
    Reset,
}

/// Kiro credential management subcommands.
#[derive(Debug, Subcommand)]
enum KiroCommand {
    #[command(
        about = "Import credentials from an existing Kiro CLI or IDE login",
        long_about = "Read a local Kiro credential store, save a rotom Kiro provider entry, and keep token values out of stdout. Use --from cli for ~/.local/share/kiro-cli/data.sqlite3 or --from desktop for ~/.aws/sso/cache/kiro-auth-token.json."
    )]
    Import {
        #[arg(long, value_name = "PATH", help = "rotom credential file to write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long = "from",
            value_name = "SOURCE",
            required = true,
            help = "Credential source: cli, desktop, or explicit auto"
        )]
        source: String,
        #[arg(
            long,
            value_name = "PATH",
            help = "Explicit Kiro SQLite database or desktop token JSON path"
        )]
        path: Option<PathBuf>,
    },
}

/// One or more socket addresses accepted by `--bind`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BindAddresses(Vec<SocketAddr>);

impl BindAddresses {
    fn into_vec(self) -> Vec<SocketAddr> {
        self.0
    }
}

impl std::str::FromStr for BindAddresses {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        parse_bind_addresses(value)
            .map(Self)
            .map_err(|error| error.to_string())
    }
}

/// Shared CLI options used by `daemon install` and `daemon reinstall`.
#[derive(Debug, Clone, Args)]
struct DaemonInstallCliOptions {
    #[arg(
        long,
        value_name = "PATH",
        help = "rotom executable to run; defaults to the current executable"
    )]
    executable: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ADDR[,ADDR...]",
        help = "Socket address or comma-separated socket addresses the daemon should listen on"
    )]
    bind: Option<BindAddresses>,
    #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
    auth_file: Option<PathBuf>,
    #[arg(
        long,
        env = "ROTOM_API_KEY",
        value_name = "KEY",
        help = "Optional local API key saved to runtime config for daemon auth"
    )]
    api_key: Option<String>,
    #[arg(
        long,
        env = "ROTOM_PROVIDER",
        value_name = "PROVIDER",
        help = "Upstream provider to serve: codex, grok, or kiro"
    )]
    provider: Option<String>,
    #[arg(
        long,
        env = "ROTOM_MODEL_FALLBACK",
        value_name = "MODEL",
        help = "Fallback model for unsupported Anthropic model ids such as claude-sonnet-*"
    )]
    model_fallback: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let log_level = LogLevel::from_verbosity(cli.verbose);
    match cli.command {
        Command::Login {
            auth_file,
            provider,
            kiro,
            originator,
        } => {
            let store = auth_store(auth_file)?;
            let provider = resolve_login_provider(&store, provider, kiro)?;
            login(store, provider, &originator).await
        }
        Command::Config { command } => config_command(command.as_ref()),
        Command::Serve {
            bind,
            auth_file,
            api_key,
            provider,
            model_fallback,
        } => {
            logging::init(log_level)?;
            let config = load_app_config()?;
            let effective_bind = if let Some(bind) = bind {
                bind.into_vec()
            } else {
                bind_from_config(config.as_ref())?.unwrap_or_else(default_binds)
            };
            let effective_auth_file = auth_file.or_else(|| config_auth_file(config.as_ref()));
            let effective_api_key =
                api_key.or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
            let effective_model_fallback = resolve_model_fallback(model_fallback, config.as_ref());
            let http = Client::new();
            let store = auth_store(effective_auth_file)?;
            let providers = resolve_served_providers(&store, provider, config.as_ref())?;
            let mut upstreams = Vec::new();
            for provider in &providers {
                let token_manager =
                    TokenManager::new_for_provider(store.clone(), *provider, http.clone());
                token_manager.credentials().await?;
                upstreams.push(UpstreamState {
                    provider: *provider,
                    token_manager,
                    client: CodexClient::new_for_provider(http.clone(), *provider),
                });
            }
            let model_list = resolve_model_list_for_providers(&providers)?;
            println!("listening on {}", format_bind_urls(&effective_bind));
            for upstream in &upstreams {
                spawn_token_expiry_display(upstream.token_manager.clone());
            }
            serve_all(
                &effective_bind,
                AppState::new_multi_with_model_fallback(
                    upstreams,
                    effective_api_key,
                    model_list,
                    effective_model_fallback,
                ),
            )
            .await
        }
        Command::Refresh {
            auth_file,
            provider,
        } => refresh(auth_store(auth_file)?, provider).await,
        Command::Status {
            auth_file,
            provider,
        } => status(auth_store(auth_file)?, provider).await,
        Command::Models { provider } => models(provider),
        Command::Kiro { command } => kiro_command(command),
        Command::Update { version } => update(version.as_deref()),
        Command::Daemon { command } => daemon_command(command, cli.verbose),
    }
}

/// Reinstalls rotom from crates.io through Cargo.
fn update(version: Option<&str>) -> Result<()> {
    let mut command = build_update_command(version);
    println!("running {command:?}");
    let status = command.status()?;
    if status.success() {
        match version {
            Some(version) => println!("updated rotom to version {version}"),
            None => println!("updated rotom to the latest published release"),
        }
        match daemon::restart_loaded() {
            Ok(true) => println!("reloaded the installed rotom daemon"),
            Ok(false) => {}
            Err(error) => eprintln!("warning: failed to reload rotom daemon after update: {error}"),
        }
        Ok(())
    } else {
        Err(Error::upstream(format!(
            "cargo install exited with status {status}"
        )))
    }
}

/// Builds the Cargo command used by `rotom update`.
#[must_use]
fn build_update_command(version: Option<&str>) -> ProcessCommand {
    let mut command = ProcessCommand::new("cargo");
    command.args(["install", "--locked", "--force", "rotom"]);
    if let Some(version) = version {
        command.args(["--version", version]);
    }
    command
}

include!("main_config.rs");

include!("main_auth.rs");

include!("main_models.rs");

/// Fetches token status and renders a human-readable report.
async fn status(store: AuthStore, provider: Option<String>) -> Result<()> {
    let http = Client::new();
    let requested_provider = provider
        .map(|value| value.parse::<Provider>())
        .transpose()?;
    let providers = resolve_status_providers(store.load_all()?, requested_provider)?;

    println!("{}", rotom_version_line());
    for provider in providers {
        println!();
        render_provider_status(&store, provider, http.clone()).await;
    }

    if let Some(base_url) = running_daemon_base_url(&http).await {
        println!();
        println!("daemon: running at {base_url}");
        println!("endpoints:");
        for line in daemon_endpoint_lines(&base_url) {
            println!("{line}");
        }
    }

    Ok(())
}

fn rotom_version_line() -> String {
    format!("rotom: {}", env!("CARGO_PKG_VERSION"))
}

async fn render_provider_status(store: &AuthStore, provider: Provider, http: Client) {
    let token_manager = TokenManager::new_for_provider(store.clone(), provider, http);
    let snapshot = token_manager.credentials_snapshot().await;
    let credentials = match token_manager.credentials().await {
        Ok(credentials) => credentials,
        Err(error) => {
            eprintln!(
                "warning: failed to refresh {} credentials: {error}",
                provider.display_name()
            );
            let Some(credentials) = snapshot else {
                println!("provider: {provider}");
                println!("token: unavailable");
                println!("status: refresh failed");
                println!("highlight_models:");
                for line in model_lines(provider_model_ids(provider)) {
                    println!("{line}");
                }
                return;
            };
            let models = provider_model_ids(provider);
            let highlights = highlight_model_ids_for_provider(provider, &models);
            println!("provider: {}", credentials.provider);
            println!(
                "token: refresh failed ({})",
                credential_subject(&credentials)
            );
            println!("status: refresh failed");
            println!("highlight_models:");
            for line in model_lines(highlights) {
                println!("{line}");
            }
            return;
        }
    };
    let models = provider_model_ids(provider);
    let highlights = highlight_model_ids_for_provider(provider, &models);
    println!("provider: {}", credentials.provider);
    println!("token: {}", token_expiry_message(&credentials));
    println!("status: authenticated");
    println!("highlight_models:");
    for line in model_lines(highlights) {
        println!("{line}");
    }
}

fn resolve_status_providers(
    credentials: Vec<Credentials>,
    requested_provider: Option<Provider>,
) -> Result<Vec<Provider>> {
    if credentials.is_empty() {
        return Err(Error::config("not logged in; run `rotom login` first"));
    }

    if let Some(provider) = requested_provider {
        return if credentials.iter().any(|item| item.provider == provider) {
            Ok(vec![provider])
        } else {
            Err(Error::config(format!(
                "not logged in for provider {provider}; run `rotom login --provider {provider}` first"
            )))
        };
    }

    let mut providers = credentials
        .into_iter()
        .map(|credentials| credentials.provider)
        .collect::<Vec<_>>();
    providers.sort_unstable();
    providers.dedup();
    Ok(providers)
}

async fn running_daemon_base_url(http: &Client) -> Option<String> {
    let config = load_app_config().ok().flatten();
    let binds = bind_from_config(config.as_ref())
        .ok()
        .flatten()
        .unwrap_or_else(default_binds);
    for bind in binds {
        let base_url = format!("http://{bind}");
        let health_url = format!("{base_url}/health");
        let Some(response) = http
            .get(health_url)
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .ok()
        else {
            continue;
        };
        if response.status().is_success() {
            return Some(base_url);
        }
    }
    None
}

fn daemon_endpoint_lines(base_url: &str) -> Vec<String> {
    const ENDPOINTS: [(&str, &str); 20] = [
        ("GET", "/health"),
        ("GET", "/v1/status"),
        ("GET", "/v1/models"),
        ("POST", "/v1/auth/refresh"),
        ("POST", "/v1/chat/completions"),
        ("POST", "/v1/responses"),
        ("GET,DELETE", "/v1/responses/{response_id}"),
        ("POST", "/v1/responses/{response_id}/cancel"),
        ("GET", "/v1/responses/{response_id}/input_items"),
        ("POST", "/v1/responses/compact"),
        ("POST", "/v1/responses/input_tokens"),
        ("POST", "/v1/messages"),
        ("POST", "/v1/messages/count_tokens"),
        ("GET,POST", "/v1/messages/batches"),
        ("GET,DELETE", "/v1/messages/batches/{batch_id}"),
        ("POST", "/v1/messages/batches/{batch_id}/cancel"),
        ("GET", "/v1/messages/batches/{batch_id}/results"),
        ("POST", "/v1/images/generations"),
        ("GET,POST", "/v1/tts"),
        ("GET", "/v1/tts/voices"),
    ];

    ENDPOINTS
        .iter()
        .map(|(method, path)| format!("  {method:<10} {base_url}{path}"))
        .collect()
}

/// Resolves the configured credential store path.
fn auth_store(path: Option<PathBuf>) -> Result<AuthStore> {
    path.map(AuthStore::new)
        .map_or_else(AuthStore::from_default_path, Ok)
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:14550"
        .parse()
        .expect("hardcoded default bind address should parse")
}

fn default_binds() -> Vec<SocketAddr> {
    vec![default_bind()]
}

fn bind_from_config(config: Option<&AppConfig>) -> Result<Option<Vec<SocketAddr>>> {
    let Some(config) = config else {
        return Ok(None);
    };
    let Some(hosts) = config.bind_host.as_deref() else {
        return Ok(None);
    };
    let Some(port) = config.bind_port else {
        return Ok(None);
    };
    parse_bind_hosts(hosts, port).map(Some)
}

fn parse_bind_addresses(value: &str) -> Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for item in split_bind_items(value) {
        push_unique_addrs(&mut addrs, parse_bind_address_item(item)?);
    }
    non_empty_bind_addrs(addrs, "bind address")
}

fn parse_bind_address_item(item: &str) -> Result<Vec<SocketAddr>> {
    if item.contains('/') {
        let (host, port) = parse_cidr_bind_address(item)?;
        return parse_bind_host_selector(host, port);
    }

    item.to_socket_addrs()
        .map(Iterator::collect)
        .map_err(|error| Error::config(format!("invalid bind address `{item}`: {error}")))
}

fn parse_cidr_bind_address(item: &str) -> Result<(&str, u16)> {
    let Some((host, port)) = item.rsplit_once(':') else {
        return Err(Error::config(format!(
            "CIDR bind address `{item}` must include a port, for example `192.168.1.0/24:14550`"
        )));
    };
    let port = port
        .parse::<u16>()
        .map_err(|error| Error::config(format!("invalid bind port `{port}`: {error}")))?;
    Ok((host, port))
}

fn parse_bind_hosts(hosts: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for host in split_bind_items(hosts) {
        push_unique_addrs(&mut addrs, parse_bind_host_selector(host, port)?);
    }
    non_empty_bind_addrs(addrs, "bind host")
}

fn parse_bind_host_selector(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if host.contains('/') {
        return expand_ipv4_cidr_bind_host(host, port);
    }

    (host, port)
        .to_socket_addrs()
        .map(Iterator::collect)
        .map_err(|error| Error::config(format!("invalid bind host `{host}`: {error}")))
}

fn expand_ipv4_cidr_bind_host(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let cidr = Ipv4Cidr::parse(host)?;
    let addrs = local_ipv4_addrs()?
        .into_iter()
        .filter(|addr| cidr.contains(*addr))
        .map(|addr| SocketAddr::from((addr, port)))
        .collect::<Vec<_>>();

    if addrs.is_empty() {
        Err(Error::config(format!(
            "bind CIDR `{host}` did not match any local IPv4 interface"
        )))
    } else {
        Ok(addrs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ipv4Cidr {
    network: u32,
    mask: u32,
}

impl Ipv4Cidr {
    fn parse(value: &str) -> Result<Self> {
        let Some((addr, prefix)) = value.split_once('/') else {
            return Err(Error::config(format!("invalid IPv4 CIDR `{value}`")));
        };
        let addr = addr.parse::<Ipv4Addr>().map_err(|error| {
            Error::config(format!("invalid IPv4 CIDR address `{addr}`: {error}"))
        })?;
        let prefix = prefix.parse::<u8>().map_err(|error| {
            Error::config(format!("invalid IPv4 CIDR prefix `{prefix}`: {error}"))
        })?;
        if prefix > 32 {
            return Err(Error::config(format!(
                "invalid IPv4 CIDR prefix `{prefix}`: must be between 0 and 32"
            )));
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        };
        Ok(Self {
            network: u32::from(addr) & mask,
            mask,
        })
    }

    fn contains(self, addr: Ipv4Addr) -> bool {
        (u32::from(addr) & self.mask) == self.network
    }
}

#[cfg(unix)]
fn local_ipv4_addrs() -> Result<Vec<Ipv4Addr>> {
    let mut ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` initializes `ifaddrs` on success. We walk the linked
    // list only while pointers are non-null and free it exactly once afterward.
    if unsafe { libc::getifaddrs(&raw mut ifaddrs) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }

    let mut addrs = Vec::new();
    let mut cursor = ifaddrs;
    while !cursor.is_null() {
        // SAFETY: `cursor` points to a valid node in the `getifaddrs` list until
        // `freeifaddrs` is called after traversal.
        let item = unsafe { &*cursor };
        if !item.ifa_addr.is_null() {
            // SAFETY: `ifa_addr` is non-null and the family check precedes the
            // cast to `sockaddr_in`.
            let sockaddr = unsafe { &*item.ifa_addr };
            if i32::from(sockaddr.sa_family) == libc::AF_INET {
                // SAFETY: AF_INET entries are represented as `sockaddr_in`.
                // `read_unaligned` avoids assuming platform alignment for the
                // generic `sockaddr` pointer.
                let addr =
                    unsafe { std::ptr::read_unaligned(item.ifa_addr.cast::<libc::sockaddr_in>()) };
                let ip = Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes());
                if !addrs.contains(&ip) {
                    addrs.push(ip);
                }
            }
        }
        cursor = item.ifa_next;
    }

    // SAFETY: `ifaddrs` was initialized by a successful `getifaddrs` call and
    // has not been freed yet.
    unsafe { libc::freeifaddrs(ifaddrs) };
    Ok(addrs)
}

#[cfg(not(unix))]
fn local_ipv4_addrs() -> Result<Vec<Ipv4Addr>> {
    Err(Error::config(
        "CIDR bind selectors require Unix local interface enumeration",
    ))
}

fn split_bind_items(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

fn push_unique_addrs(addrs: &mut Vec<SocketAddr>, resolved: impl IntoIterator<Item = SocketAddr>) {
    for addr in resolved {
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
}

fn non_empty_bind_addrs(addrs: Vec<SocketAddr>, label: &str) -> Result<Vec<SocketAddr>> {
    if addrs.is_empty() {
        Err(Error::config(format!("{label} list must not be empty")))
    } else {
        Ok(addrs)
    }
}

fn format_bind_addresses(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn format_bind_urls(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(|addr| format!("http://{addr}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn config_auth_file(config: Option<&AppConfig>) -> Option<PathBuf> {
    config.and_then(|item| item.auth_file.clone())
}

/// Periodically prints token expiry state while the local server is running.
fn spawn_token_expiry_display(token_manager: TokenManager) {
    tokio::spawn(async move {
        let interactive = io::stdout().is_terminal();
        let mut ticker = interval(if interactive {
            INTERACTIVE_TOKEN_STATUS_INTERVAL
        } else {
            LOG_TOKEN_STATUS_INTERVAL
        });
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let status = token_expiry_status(&token_manager).await;
            if interactive {
                print!("\r\x1b[2K{status}");
                let _ = io::stdout().flush();
            } else {
                println!("{status}");
            }
        }
    });
}

/// Builds a one-line token expiry status string for the current credentials snapshot.
async fn token_expiry_status(token_manager: &TokenManager) -> String {
    match token_manager.credentials_snapshot().await {
        Some(credentials) if credentials.expires_at > now_unix() => {
            token_expiry_message(&credentials)
        }
        Some(_) => token_manager.credentials().await.map_or_else(
            |error| format!("token refresh failed: {error}"),
            |credentials| token_expiry_message(&credentials),
        ),
        None => "token status unavailable: not logged in; run `rotom login` first".to_owned(),
    }
}

/// Renders a human-readable expiry message for one credential set.
fn token_expiry_message(credentials: &Credentials) -> String {
    let remaining_secs = credentials.expires_at.saturating_sub(now_unix());
    let subject = credential_subject(credentials);
    if remaining_secs == 0 {
        format!("token expired ({subject})")
    } else {
        format!(
            "token expires in {} ({subject})",
            format_duration(remaining_secs),
        )
    }
}

fn credential_subject(credentials: &Credentials) -> String {
    format!("{} credentials", credentials.provider.display_name())
}

#[cfg(test)]
mod main_tests;
