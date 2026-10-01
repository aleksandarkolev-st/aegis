use std::{error::Error, sync::Arc, time::Duration};

use aegis_relay::{
    config::Config,
    http::{HttpState, router},
    provider::EvolutionAdapter,
    repository::{PgRepository, RelayRepository},
    service::RelayService,
    transport::JetStreamTransport,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let config = Config::from_env()?;
    let repository = PgRepository::connect(&config.database_url).await?;
    let cleanup_repository = repository.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            if cleanup_repository.prune_expired_receipts().await.is_err() {
                tracing::warn!("delivery receipt cleanup failed");
            }
        }
    });
    let transport = Arc::new(
        JetStreamTransport::connect(
            &config.nats_url,
            &config.nats_auth_token,
            config.nats_tls_root_cert.as_deref(),
        )
        .await?,
    );
    let provider = Arc::new(EvolutionAdapter::new(
        &config.evolution_base_url,
        config.evolution_instance,
        config.evolution_api_key,
        config.evolution_webhook_secret,
    )?);
    let service = Arc::new(RelayService::new(
        Arc::new(repository.clone()),
        transport.clone(),
        provider,
    ));
    let http_state = HttpState {
        service: service.clone(),
        repository,
        admin_token: Arc::new(config.admin_token),
    };
    let listener = TcpListener::bind(config.bind_addr).await?;
    let address = listener.local_addr()?;
    tracing::info!(%address, "Aegis relay listening");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let event_consumer = tokio::spawn(transport.supervise_events(service, async move {
        let _ = shutdown_rx.await;
    }));
    let server_result = axum::serve(listener, router(http_state))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    let _ = shutdown_tx.send(());
    if let Err(error) = event_consumer.await {
        tracing::warn!(%error, "JetStream event supervisor stopped unexpectedly");
    }
    server_result?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown requested");
}
