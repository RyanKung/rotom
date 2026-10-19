//! Config-home serialization and service startup regression witnesses.

#[cfg(unix)]
use super::{explicit_rotom_home, rotom_home};
use super::{launchd_plist, systemd_unit, tests::options};
#[cfg(unix)]
use crate::Error;
use crate::Result;
use std::path::Path;
#[cfg(unix)]
use std::{env, process::Command};

/// Both service formats preserve path punctuation without expanding variables.
#[test]
fn service_config_home_escapes_path_punctuation() -> Result<()> {
    // These characters exercise XML entities, systemd quoting, and specifiers.
    let home = Path::new("/tmp/config 'quoted\" \\ %n $HOME & <dir>");
    let plist = launchd_plist(&options(), home, Some(home));
    let unit = systemd_unit(&options(), Some(home))?;

    assert!(plist.contains("<key>EnvironmentVariables</key>"));
    assert!(
        plist.contains(
            "<string>/tmp/config &apos;quoted&quot; \\ %n $HOME &amp; &lt;dir&gt;</string>"
        )
    );
    assert!(
        unit.contains("Environment=\"ROTOM_HOME=/tmp/config 'quoted\\\" \\\\ %%n $HOME & <dir>\"")
    );
    Ok(())
}

/// Environment values cannot inject additional systemd directives or controls.
#[test]
fn systemd_config_home_rejects_control_characters() {
    for home in ["/tmp/a\nRestart=no", "/tmp/a\r", "/tmp/a\t", "/tmp/a\u{7f}"] {
        assert!(systemd_unit(&options(), Some(Path::new(home))).is_err());
    }
}

/// A relative custom home survives service startup without the install environment.
#[cfg(unix)]
#[test]
fn custom_config_home_survives_clean_service_environment() -> Result<()> {
    // The child owns all config writes under this isolated installation directory.
    let directory = crate::testsupport::TempDir::new()?;
    let output = Command::new(env::current_exe()?)
        .args([
            "--exact",
            "daemon::home_tests::config_home_child",
            "--nocapture",
        ])
        .current_dir(directory.path())
        .env("ROTOM_DAEMON_HOME_TEST", "install")
        .env("ROTOM_HOME", "relative config")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("verified service config home"));
    Ok(())
}

/// Re-enters installation or runtime selection with only per-process environment changes.
#[cfg(unix)]
#[test]
fn config_home_child() -> Result<()> {
    // Ordinary discovery leaves this isolated subprocess witness dormant.
    let Ok(stage) = env::var("ROTOM_DAEMON_HOME_TEST") else {
        return Ok(());
    };
    // This is the production runtime config boundary, evaluated in each process.
    let store = crate::config::AppConfigStore::from_default_path()?;
    if stage == "runtime" {
        let loaded = store.load()?;
        assert_eq!(
            loaded.and_then(|config| config.model_fallback).as_deref(),
            Some("gpt-6.1-sol")
        );
        println!("verified runtime config home");
        return Ok(());
    }
    // The directory need not exist when its absolute identity is captured.
    let home = rotom_home()?;
    assert_eq!(home, env::current_dir()?.join("relative config"));
    assert_eq!(explicit_rotom_home()?.as_deref(), Some(home.as_path()));
    assert!(!home.exists());
    store.save(&crate::config::AppConfig {
        model_fallback: Some("gpt-6.1-sol".to_owned()),
        ..crate::config::AppConfig::default()
    })?;
    // Extract the uncomplicated test path from each generated service format.
    let plist = launchd_plist(&options(), &home, Some(&home));
    let unit = systemd_unit(&options(), Some(&home))?;
    let launchd_home = plist
        .split_once("<key>ROTOM_HOME</key>\n        <string>")
        .and_then(|(_, suffix)| suffix.split_once("</string>"))
        .map(|(value, _)| value)
        .ok_or_else(|| Error::config("missing LaunchAgent config home"))?;
    let systemd_home = unit
        .lines()
        .find_map(|line| line.strip_prefix("Environment=\"ROTOM_HOME="))
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| Error::config("missing systemd config home"))?;
    for serialized_home in [launchd_home, systemd_home] {
        // Clear the shell-selected home; restore only what the service persisted.
        let output = Command::new(env::current_exe()?)
            .args([
                "--exact",
                "daemon::home_tests::config_home_child",
                "--nocapture",
            ])
            .current_dir(env::temp_dir())
            .env_remove("ROTOM_HOME")
            .env("ROTOM_HOME", serialized_home)
            .env("ROTOM_DAEMON_HOME_TEST", "runtime")
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("verified runtime config home"));
    }
    println!("verified service config home");
    Ok(())
}
