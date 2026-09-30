use std::{env, net::SocketAddr, path::PathBuf};

use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("environment variable {0} is invalid")]
    Invalid(&'static str),
}

#[derive(Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub nats_url: String,
    pub nats_auth_token: String,
    pub nats_tls_root_cert: Option<PathBuf>,
    pub admin_token: String,
    pub evolution_base_url: String,
    pub evolution_instance: String,
    pub evolution_api_key: String,
    pub evolution_webhook_secret: Vec<u8>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = env::var("RELAY_BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
            .parse()
            .map_err(|_| ConfigError::Invalid("RELAY_BIND_ADDR"))?;
        let database_url = required("DATABASE_URL")?;
        let nats_url = required("NATS_URL")?;
        let nats_auth_token = required("NATS_AUTH_TOKEN")?;
        let nats_tls_root_cert = env::var_os("NATS_TLS_ROOT_CERT").map(PathBuf::from);
        let admin_token = required("RELAY_ADMIN_TOKEN")?;
        let evolution_base_url = required("EVOLUTION_BASE_URL")?;
        let evolution_instance = required("EVOLUTION_INSTANCE")?;
        let evolution_api_key = required("EVOLUTION_API_KEY")?;
        let evolution_webhook_secret = required("EVOLUTION_WEBHOOK_SECRET")?.into_bytes();

        if admin_token.len() < 32 {
            return Err(ConfigError::Invalid("RELAY_ADMIN_TOKEN"));
        }
        if evolution_webhook_secret.len() < 32 {
            return Err(ConfigError::Invalid("EVOLUTION_WEBHOOK_SECRET"));
        }
        if !nats_url.starts_with("tls://") {
            return Err(ConfigError::Invalid("NATS_URL (must use tls://)"));
        }
        if nats_auth_token.len() < 32 {
            return Err(ConfigError::Invalid("NATS_AUTH_TOKEN"));
        }
        let database_is_tls = database_url.contains("sslmode=require")
            || database_url.contains("sslmode=verify-ca")
            || database_url.contains("sslmode=verify-full");
        let allow_local_plaintext = env::var("RELAY_ALLOW_INSECURE_LOCAL_DATABASE")
            .is_ok_and(|value| value == "true")
            && Url::parse(&database_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| {
                    matches!(host.as_str(), "localhost" | "127.0.0.1" | "postgres")
                });
        if !database_is_tls && !allow_local_plaintext {
            return Err(ConfigError::Invalid("DATABASE_URL (must require TLS)"));
        }

        Ok(Self {
            bind_addr,
            database_url,
            nats_url,
            nats_auth_token,
            nats_tls_root_cert,
            admin_token,
            evolution_base_url,
            evolution_instance,
            evolution_api_key,
            evolution_webhook_secret,
        })
    }
}

fn required(key: &'static str) -> Result<String, ConfigError> {
    env::var(key).map_err(|_| ConfigError::Missing(key))
}
