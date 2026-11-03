//! Local OAuth application configuration; no application secrets are compiled in.
use crate::{Error, Result};
use serde::Deserialize;
use std::{env, path::PathBuf};

#[derive(Deserialize)]
pub(super) struct Application {
    pub(super) client_id: String,
    pub(super) client_secret: String,
}

pub(super) fn load() -> Result<Application> {
    let id = env::var("ROTOM_GEMINI_CLIENT_ID").ok();
    let secret = env::var("ROTOM_GEMINI_CLIENT_SECRET").ok();
    let application = match (id, secret) {
        (Some(client_id), Some(client_secret)) => Application {
            client_id,
            client_secret,
        },
        (None, None) => {
            let home = env::var("ROTOM_HOME")
                .map(PathBuf::from)
                .or_else(|_| env::var("HOME").map(|home| PathBuf::from(home).join(".rotom")))
                .map_err(|_| {
                    Error::config("set ROTOM_HOME or HOME for Gemini OAuth configuration")
                })?;
            let bytes = std::fs::read(home.join("gemini-oauth.json")).map_err(|_| Error::config(
                "configure gemini-oauth.json or both ROTOM_GEMINI_CLIENT_ID and ROTOM_GEMINI_CLIENT_SECRET before Gemini login/refresh"))?;
            serde_json::from_slice::<Application>(&bytes)
                .map_err(|_| Error::config("invalid Gemini OAuth application configuration"))?
        }
        _ => {
            return Err(Error::config(
                "set both ROTOM_GEMINI_CLIENT_ID and ROTOM_GEMINI_CLIENT_SECRET",
            ));
        }
    };
    validate(application)
}

fn validate(application: Application) -> Result<Application> {
    if application.client_id.trim().is_empty() || application.client_secret.trim().is_empty() {
        return Err(Error::config(
            "Gemini OAuth client ID and secret must not be empty",
        ));
    }
    Ok(application)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_application_configuration_is_rejected() {
        for (id, secret) in [("", "secret"), ("id", " "), ("", "")] {
            assert!(
                validate(Application {
                    client_id: id.into(),
                    client_secret: secret.into()
                })
                .is_err()
            );
        }
        assert!(
            validate(Application {
                client_id: "test-id".into(),
                client_secret: "test-secret".into()
            })
            .is_ok()
        );
    }
}
