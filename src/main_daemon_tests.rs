//! Regression coverage for daemon install overrides and runtime fallback changes.

use super::testsupport::TempDir;
use crate::{
    AppConfig, AppConfigStore, Cli, Command, DEFAULT_MODEL_FALLBACK, DaemonCommand,
    resolve_daemon_install_options, resolve_model_fallback,
};
use clap::Parser;
use rotom::{Error, Result};
use std::{env, process::Command as ProcessCommand};

/// Selects the isolated child case without changing the test runner's environment.
const FALLBACK_TEST_CASE: &str = "ROTOM_DAEMON_FALLBACK_TEST_CASE";

/// A service without an install override observes subsequent config/default changes.
#[test]
fn daemon_without_override_uses_current_runtime_config_and_default() -> Result<()> {
    run_daemon_fallback_case("runtime", None)
}

/// An unset installation config must not pin the binary's installation-time default.
#[test]
fn daemon_without_config_fallback_defers_to_runtime_resolution() -> Result<()> {
    run_daemon_fallback_case("default", None)
}

/// A CLI override is retained even when the environment and runtime config differ.
#[test]
fn daemon_cli_fallback_overrides_environment_and_runtime_config() -> Result<()> {
    run_daemon_fallback_case("cli", Some("gpt-6-sol"))
}

/// An environment override at installation is an intentional persistent selection.
#[test]
fn daemon_environment_fallback_overrides_runtime_config() -> Result<()> {
    run_daemon_fallback_case("environment", Some("gpt-6-sol"))
}

/// Runs real clap environment parsing in a child with an isolated config directory.
///
/// Only the child's environment is changed, so parallel tests and the user's
/// config remain untouched. No service-manager operation is performed.
fn run_daemon_fallback_case(case: &str, environment_override: Option<&str>) -> Result<()> {
    // The temporary home owns every config write performed by the child.
    let home = TempDir::new()?;
    // Re-enter only the regression witness, with unrelated install inputs cleared.
    let mut child = ProcessCommand::new(env::current_exe()?);
    child
        .args([
            "--exact",
            "main_tests::daemon_defaults::daemon_fallback_child",
            "--nocapture",
        ])
        .env(FALLBACK_TEST_CASE, case)
        .env("ROTOM_HOME", home.path())
        .env_remove("ROTOM_API_KEY")
        .env_remove("ROTOM_PROVIDER")
        .env_remove("ROTOM_MODEL_FALLBACK");
    if let Some(model) = environment_override {
        child.env("ROTOM_MODEL_FALLBACK", model);
    }
    // Captured output explains a failed child assertion without hiding the case.
    let output = child.output()?;
    assert!(
        output.status.success(),
        "fallback case {case} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // A successful zero-test invocation must not accidentally satisfy the witness.
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("verified daemon fallback case: {case}"))
    );
    Ok(())
}

/// Exercises installation and later runtime resolution in the isolated child.
#[test]
fn daemon_fallback_child() -> Result<()> {
    // Ordinary test discovery leaves the child dormant; the parent sets the case.
    let Ok(case) = env::var(FALLBACK_TEST_CASE) else {
        return Ok(());
    };
    // The parent sets ROTOM_HOME before this process starts, avoiding global races.
    let store = AppConfigStore::from_default_path()?;
    // Model the stale default that was present when an older service was installed.
    let mut config = AppConfig {
        model_fallback: (case != "default").then(|| "gpt-5.5".to_owned()),
        ..AppConfig::default()
    };
    store.save(&config)?;

    // Use the real install CLI, with a fixed executable and bind for reproducibility.
    let mut arguments = vec![
        "rotom",
        "daemon",
        "install",
        "--executable",
        "/test/rotom",
        "--bind",
        "127.0.0.1:14550",
    ];
    // Each case states which explicit source, if any, survives installation.
    let expected_override = match case.as_str() {
        "runtime" | "default" => None,
        "environment" => Some("gpt-6-sol"),
        "cli" => {
            arguments.extend(["--model-fallback", "gpt-6-astra"]);
            Some("gpt-6-astra")
        }
        _ => return Err(Error::config("unknown isolated fallback test case")),
    };
    // Parse and resolve through the production boundary without installing a service.
    let cli = Cli::try_parse_from(arguments).map_err(|error| Error::config(error.to_string()))?;
    let Command::Daemon {
        command: DaemonCommand::Install(options),
    } = cli.command
    else {
        return Err(Error::config("expected daemon install test arguments"));
    };
    let installed = resolve_daemon_install_options(options, 0)?;
    assert_eq!(installed.model_fallback.as_deref(), expected_override);

    // A later process reads changed config while retaining the same installed argv.
    config.model_fallback = Some("gpt-6.1-sol".to_owned());
    store.save(&config)?;
    let changed_config = store.load()?;
    assert_eq!(
        resolve_model_fallback(installed.model_fallback.clone(), changed_config.as_ref())
            .as_deref(),
        Some(expected_override.unwrap_or("gpt-6.1-sol")),
    );

    // Removing the config selection exposes the running binary's current default.
    config.model_fallback = None;
    store.save(&config)?;
    let unset_config = store.load()?;
    assert_eq!(
        resolve_model_fallback(installed.model_fallback, unset_config.as_ref()).as_deref(),
        Some(expected_override.unwrap_or(DEFAULT_MODEL_FALLBACK)),
    );
    println!("verified daemon fallback case: {case}");
    Ok(())
}
