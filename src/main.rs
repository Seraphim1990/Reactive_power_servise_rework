mod caluculation;
mod logger;
mod meta;
mod transport;

use caluculation::{
    actor_loop::reactive_loop,
    handoff::HandoffCoordinator,
    settings::{AuxiliarySettings, CondensatorsSettings, KguSettings, Settings},
};
use std::{
    error::Error,
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};
use tokio::sync::mpsc;
use transport::{
    post::poster_loop::WriteMaster,
    web_api_actor::{ApiClient, Config as ApiConfig},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let batteries_cfg = CondensatorsSettings::load_or_create("config/condensators.toml")?;
    let kgu_cfg = KguSettings::load_or_create("config/kgu.toml")?;
    let auxiliary_cfg = AuxiliarySettings::load_or_create("config/auxiliary.toml")?;
    let api_config = ApiConfig::load("config/settings.ini").map_err(std::io::Error::other)?;
    let logger = logger::printers::init_logger();
    let ids = Arc::new(AtomicU64::new(1));
    let batteries = batteries_cfg.into_actor(ids.clone(), Instant::now());
    let (generator, remote, trend) = kgu_cfg.into_actor(ids.clone());
    let auxiliary = auxiliary_cfg.into_actor(ids);
    let coordinator = HandoffCoordinator::new(batteries, generator, remote, auxiliary, trend);

    let (telemetry_tx, telemetry_rx) = mpsc::unbounded_channel();
    let (write_tx, write_rx) = mpsc::channel(64);
    let mut writer = tokio::spawn(async move { WriteMaster::new().run(write_rx).await });
    let mut control = tokio::spawn(reactive_loop(telemetry_rx, write_tx, coordinator));
    let (api, mut api_task) = ApiClient::spawn(api_config, telemetry_tx);
    api.run_api_worker();

    // A panic in an unfinished API hook must stop the other workers too.
    let result = tokio::select! {
        signal = tokio::signal::ctrl_c() => signal.map_err(|e| e.to_string()),
        result = &mut api_task => Err(format!("API worker stopped: {result:?}")),
        result = &mut writer => Err(format!("Modbus worker stopped: {result:?}")),
        result = &mut control => Err(format!("Control worker stopped: {result:?}")),
    };
    api_task.abort();
    control.abort();
    writer.abort();
    drop(api);
    logger.abort();
    result.map_err(|e| std::io::Error::other(e).into())
}
