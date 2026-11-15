mod caluculation;
mod logger;
mod meta;
mod transport;

use caluculation::{
    actor_loop::reactive_loop,
    auxiliary::AuxiliaryStation,
    condesators::{
        condensators_actor::{ActorConfig, CondensatorActor, Target},
        condensators_state::CondensatorState,
    },
    generator::generator_actor::{GenTarget, GeneratorActor, GeneratorConfig},
    handoff::HandoffCoordinator,
    power_trend::PowerTrendTracker,
};
use meta::state::command::WriteCommand;
use serde::Deserialize;
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

#[derive(Deserialize)]
struct Device {
    ip: String,
    port: u16,
    unit: u8,
    register: u16,
    #[serde(default)]
    single_write: bool,
}
impl Device {
    fn target(&self) -> Target {
        Target {
            ip: Arc::new(self.ip.clone()),
            port: self.port,
            unit: self.unit as u16,
            register: self.register,
            write: if self.single_write {
                WriteCommand::Single
            } else {
                WriteCommand::Multi
            },
        }
    }
}

#[derive(Deserialize)]
struct RuntimeConfig {
    api_settings: String,
    condensators_settings: String,
    trend_capacity: usize,
    trend_noise_kw: f32,
    generator_remote_register: u16,
    generator_carriage: bool,
    auxiliary_on_value: u16,
    auxiliary_off_value: u16,
    batteries: Device,
    generator: Device,
    auxiliary: Device,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/runtime.toml".into());
    let cfg: RuntimeConfig = toml::from_str(&std::fs::read_to_string(&cfg_path)?)?;
    if cfg.trend_capacity < 3 || !cfg.trend_noise_kw.is_finite() || cfg.trend_noise_kw < 0.0 {
        return Err(std::io::Error::other("Некоректні параметри тренду").into());
    }
    let state = CondensatorState::load(&cfg.condensators_settings)?;
    let api_config = ApiConfig::load(&cfg.api_settings).map_err(std::io::Error::other)?;
    let logger = logger::printers::init_logger();
    let ids = Arc::new(AtomicU64::new(1));
    let batteries = CondensatorActor::new(
        state,
        ActorConfig::default(),
        cfg.batteries.target(),
        ids.clone(),
        Instant::now(),
    );
    let gen_target = cfg.generator.target();
    let generator = GeneratorActor::new(
        GeneratorConfig::default(),
        GenTarget {
            ip: gen_target.ip,
            port: gen_target.port,
            unit: gen_target.unit,
            setpoint_register: gen_target.register,
            remote_register: cfg.generator_remote_register,
            write: gen_target.write,
        },
        ids.clone(),
        cfg.generator_carriage,
    );
    let auxiliary = AuxiliaryStation {
        target: cfg.auxiliary.target(),
        ids,
        on_value: cfg.auxiliary_on_value,
        off_value: cfg.auxiliary_off_value,
    };
    let coordinator = HandoffCoordinator::new(
        batteries,
        generator,
        auxiliary,
        PowerTrendTracker::new(cfg.trend_capacity, cfg.trend_noise_kw),
    );

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
