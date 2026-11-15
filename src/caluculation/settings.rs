//! Three actor-owned configuration documents. Runtime wiring stays in main.
use super::{
    auxiliary::AuxiliaryStation,
    condesators::{
        condensators_actor::{ActorConfig, CondensatorActor, Target},
        condensators_state::CondensatorState,
    },
    generator::generator_actor::{GenTarget, GeneratorActor, GeneratorConfig},
    power_trend::PowerTrendTracker,
};
use crate::meta::state::command::WriteCommand;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn check(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::other(message).into())
    }
}

pub trait Settings: Serialize + DeserializeOwned + Default {
    fn validate(&self) -> Result<()>;
    fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Self = toml::from_str(&text)?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate()?;
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSettings {
    pub ip: String,
    pub port: u16,
    pub unit: u8,
    pub register: u16,
    #[serde(default)]
    pub single_write: bool,
}
impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            ip: "127.0.0.1".into(),
            port: 502,
            unit: 1,
            register: 0,
            single_write: true,
        }
    }
}
impl DeviceSettings {
    fn validate(&self) -> Result<()> {
        self.ip.parse::<std::net::IpAddr>()?;
        check(self.port > 0, "Modbus port must be nonzero")
    }
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

#[derive(Serialize, Deserialize)]
pub struct CondensatorsSettings {
    #[serde(flatten)]
    pub state: CondensatorState,
    pub target: DeviceSettings,
    #[serde(default)]
    pub actor: ActorConfig,
}
impl Default for CondensatorsSettings {
    fn default() -> Self {
        Self {
            state: CondensatorState::default(),
            actor: ActorConfig::default(),
            target: DeviceSettings {
                ip: "192.168.2.211".into(),
                register: 470,
                single_write: false,
                ..Default::default()
            },
        }
    }
}
impl Settings for CondensatorsSettings {
    fn validate(&self) -> Result<()> {
        self.state.validate()?;
        self.target.validate()?;
        check(
            self.state.bank_voltage_nom.is_finite() && self.state.bank_voltage_nom > 0.0,
            "bank_voltage_nom must be finite and positive",
        )?;
        check(
            (1..=10).contains(&self.actor.settle_samples) && self.actor.failsafe_exit_samples > 0,
            "settle_samples must be 1..10 and failsafe_exit_samples > 0",
        )?;
        check(
            self.actor.shed_q_tolerance_steps.is_finite()
                && self.actor.shed_q_tolerance_steps >= 0.0,
            "shed_q_tolerance_steps must be finite and nonnegative",
        )
    }
}
impl CondensatorsSettings {
    pub fn into_actor(self, ids: Arc<AtomicU64>, now: Instant) -> CondensatorActor {
        CondensatorActor::new(self.state, self.actor, self.target.target(), ids, now)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KguSettings {
    pub target: DeviceSettings,
    pub remote_register: u16,
    pub use_carriage: bool,
    pub trend_capacity: usize,
    pub trend_noise_kw: f32,
    #[serde(default)]
    pub actor: GeneratorConfig,
}
impl Default for KguSettings {
    fn default() -> Self {
        Self {
            target: DeviceSettings {
                register: 8,
                ..Default::default()
            },
            remote_register: 7,
            use_carriage: true,
            trend_capacity: 5,
            trend_noise_kw: 2.0,
            actor: GeneratorConfig::default(),
        }
    }
}
impl Settings for KguSettings {
    fn validate(&self) -> Result<()> {
        self.target.validate()?;
        check(
            self.trend_capacity >= 3
                && self.trend_noise_kw.is_finite()
                && self.trend_noise_kw >= 0.0,
            "invalid power trend settings",
        )?;
        check(
            self.actor.p_min_listen_kw.is_finite() && self.actor.p_min_listen_kw > 0.0,
            "p_min_listen_kw must be finite and positive",
        )?;
        let trim = &self.actor.trim;
        check(
            trim.target_kvar.is_finite()
                && trim.deadband_floor_kvar.is_finite()
                && trim.deadband_floor_kvar >= 0.0
                && (1..=200).contains(&trim.guard_units)
                && (1..=1000).contains(&trim.max_step_units)
                && (1..=1000).contains(&self.actor.carriage_step_units),
            "invalid generator trim settings",
        )
    }
}
impl KguSettings {
    pub fn into_actor(self, ids: Arc<AtomicU64>) -> (GeneratorActor, PowerTrendTracker) {
        let target = self.target.target();
        let actor = GeneratorActor::new(
            self.actor,
            GenTarget {
                ip: target.ip,
                port: target.port,
                unit: target.unit,
                setpoint_register: target.register,
                remote_register: self.remote_register,
                write: target.write,
            },
            ids,
            self.use_carriage,
        );
        (
            actor,
            PowerTrendTracker::new(self.trend_capacity, self.trend_noise_kw),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliarySettings {
    pub target: DeviceSettings,
    pub on_value: u16,
    pub off_value: u16,
}
impl Default for AuxiliarySettings {
    fn default() -> Self {
        Self {
            target: DeviceSettings {
                unit: 2,
                ..Default::default()
            },
            on_value: 1,
            off_value: 0,
        }
    }
}
impl Settings for AuxiliarySettings {
    fn validate(&self) -> Result<()> {
        self.target.validate()?;
        check(
            self.on_value != self.off_value,
            "auxiliary on/off values must differ",
        )
    }
}
impl AuxiliarySettings {
    pub fn into_actor(self, ids: Arc<AtomicU64>) -> AuxiliaryStation {
        AuxiliaryStation {
            target: self.target.target(),
            on_value: self.on_value,
            off_value: self.off_value,
            ids,
        }
    }
}

/// TOML durations are numeric seconds, e.g. step_timeout = 8.0.
pub(crate) mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(value.as_secs_f64())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let secs = f64::deserialize(deserializer)?;
        if !secs.is_finite() || secs <= 0.0 || secs > 86400.0 {
            return Err(serde::de::Error::custom(
                "duration must be > 0 and <= 86400 seconds",
            ));
        }
        Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_in_actor_configs_load_and_roundtrip() {
        let c: CondensatorsSettings =
            toml::from_str(include_str!("../../config/condensators.toml")).unwrap();
        c.validate().unwrap();
        assert_eq!(c.target.register, 470);
        let c: CondensatorsSettings = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(c.actor.step_timeout.as_secs(), 8);
        let k: KguSettings = toml::from_str(include_str!("../../config/kgu.toml")).unwrap();
        k.validate().unwrap();
        let k: KguSettings = toml::from_str(&toml::to_string(&k).unwrap()).unwrap();
        assert_eq!(k.target.register, 8);
        assert_eq!(k.remote_register, 7);
        let a: AuxiliarySettings =
            toml::from_str(include_str!("../../config/auxiliary.toml")).unwrap();
        a.validate().unwrap();
        let a: AuxiliarySettings = toml::from_str(&toml::to_string(&a).unwrap()).unwrap();
        assert_ne!(a.on_value, a.off_value);
    }
    #[test]
    fn missing_config_created_and_corrupt_config_preserved() {
        let dir = std::env::temp_dir().join(format!("reactive_settings_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for name in ["condensators", "kgu", "auxiliary"] {
            let path = dir.join(format!("{name}.toml"));
            match name {
                "condensators" => {
                    CondensatorsSettings::load_or_create(&path).unwrap();
                }
                "kgu" => {
                    KguSettings::load_or_create(&path).unwrap();
                }
                _ => {
                    AuxiliarySettings::load_or_create(&path).unwrap();
                }
            }
            assert!(path.exists());
        }
        let path = dir.join("kgu.toml");
        fs::write(&path, "broken = [").unwrap();
        assert!(KguSettings::load_or_create(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "broken = [");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rejects_invalid_timers_trend_and_device_values() {
        let invalid = include_str!("../../config/kgu.toml")
            .replace("step_timeout = 8.0", "step_timeout = -1.0");
        assert!(toml::from_str::<KguSettings>(&invalid).is_err());
        let mut k = KguSettings::default();
        k.trend_capacity = 0;
        assert!(k.validate().is_err());
        let mut c = CondensatorsSettings::default();
        c.state.bank_voltage_nom = 0.0;
        assert!(c.validate().is_err());
        let mut a = AuxiliarySettings::default();
        a.off_value = a.on_value;
        assert!(a.validate().is_err());
    }
}
