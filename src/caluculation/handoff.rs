//! Routes telemetry to the actors; returned commands must go to WriteMaster.
//! API tag names are deliberately outside this module: map them to MainDataSignal.
use super::condesators::condensators_actor::CondensatorActor;
use super::generator::{generator_actor::GeneratorActor, remote_actor::RemoteActor};
use super::grid::{GridMode, GridMonitor};
use super::{
    auxiliary::AuxiliaryStation,
    power_trend::{PowerTrendTracker, Trend},
};
use crate::logger::printers;
use crate::meta::state::{
    command::{CommandCallback, ModbusCommand},
    main_data::MainDataSignal,
};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompensationMode {
    Batteries,
    HandoffToGenerator,
    Generator,
}

pub struct HandoffCoordinator {
    batteries: CondensatorActor,
    generator: GeneratorActor,
    remote: RemoteActor,
    auxiliary: AuxiliaryStation,
    trend: PowerTrendTracker,
    mode: CompensationMode,
    engine_run: bool,
    auxiliary_requested: Option<bool>,
    grid: GridMonitor,
    applied_grid: GridMode,
    last_off: Option<Instant>,
    relay_mask: Option<u16>,
    auxiliary_off: bool,
    gen_power: Option<f64>,
}

impl HandoffCoordinator {
    pub fn new(
        batteries: CondensatorActor,
        generator: GeneratorActor,
        remote: RemoteActor,
        auxiliary: AuxiliaryStation,
        trend: PowerTrendTracker,
        grid: GridMonitor,
    ) -> Self {
        Self {
            batteries,
            generator,
            remote,
            auxiliary,
            trend,
            mode: CompensationMode::Batteries,
            engine_run: false,
            auxiliary_requested: None,
            grid,
            applied_grid: GridMode::Grid,
            last_off: None,
            relay_mask: None,
            auxiliary_off: false,
            gen_power: None,
        }
    }

    pub fn mode(&self) -> CompensationMode {
        self.mode
    }

    pub fn handle(&mut self, now: Instant, event: MainDataSignal) -> Vec<ModbusCommand> {
        let mut out = Vec::new();
        self.grid.tick(now);
        match &event {
            MainDataSignal::MainP(p) => self.grid.power(now, *p),
            MainDataSignal::MainQ(q) => self.grid.reactive(now, *q),
            _ => {}
        }
        self.sync_grid(now, &mut out);
        match event {
            MainDataSignal::MiddleVoltage(v) => self.batteries.set_voltage(now, v),
            MainDataSignal::MainP(_) => {}
            MainDataSignal::MainQ(q) => {
                if self.grid.mode() != GridMode::Grid {
                    return out;
                }
                if self.mode != CompensationMode::Batteries {
                    out.extend(self.generator.handle_q(now, q));
                }
                if self.mode != CompensationMode::Generator {
                    out.extend(self.batteries.handle_q(now, q));
                }
            }
            MainDataSignal::CondensatorsRelayState(v) => {
                if !v.is_finite() || v < 0.0 || v > u16::MAX as f64 || v.fract() != 0.0 {
                    return out;
                }
                self.relay_mask = Some(v as u16);
                out.extend(self.batteries.set_mask(now, v as u16));
                if self.grid.mode() != GridMode::Grid {
                    return out;
                }
                if self.mode == CompensationMode::HandoffToGenerator {
                    if v == 0.0 {
                        self.batteries.pause_regulation(now);
                        self.mode = CompensationMode::Generator;
                    } else if self.batteries.phase_name() == "Idle" {
                        // The transition may have been requested before the first relay read.
                        out.extend(self.batteries.start_soft_shutdown(now));
                    }
                }
            }
            MainDataSignal::GenActivePower(p) => {
                if !p.is_finite() || !(p as f32).is_finite() {
                    return out;
                }
                self.gen_power = Some(p);
                self.generator.set_active_power(now, p);
                if !self.engine_run || self.grid.mode() != GridMode::Grid {
                    return out;
                }
                let trend = self.trend.push(p as f32);
                if p <= 400.0 && trend == Trend::PowerDown {
                    self.use_batteries(now, &mut out);
                } else if p > 420.0
                    && trend == Trend::PowerUp
                    && self.mode == CompensationMode::Batteries
                {
                    self.generator.pause_regulation();
                    self.mode = CompensationMode::HandoffToGenerator;
                    self.batteries.set_allow_turn_on(false);
                    out.push(self.auxiliary.off());
                    self.auxiliary_requested = Some(false);
                    out.extend(self.batteries.start_soft_shutdown(now));
                    // Wait for a NEW zero relay read before stopping Q delivery.
                }
            }
            MainDataSignal::GenEngineRun(v) if v == 0.0 || v == 1.0 => {
                let on = v == 1.0;
                if on != self.engine_run {
                    self.trend.clear();
                }
                self.engine_run = on;
                self.batteries.set_kgu(on);
                out.extend(self.generator.set_engine_run(now, on));
                if !on {
                    self.use_batteries(now, &mut out);
                }
            }
            MainDataSignal::GenSetterLocalRemoteState(v) => {
                if v.is_finite() && (0.0..=u16::MAX as f64).contains(&v) && v.fract() == 0.0 {
                    self.generator.set_remote(v == 1.0);
                    out.extend(self.remote.set_readback(now, v as u16));
                }
            }
            MainDataSignal::GenCosSetpointOutput(v) => {
                if v.is_finite() && (0.0..=1000.0).contains(&v) && v.fract() == 0.0 {
                    self.generator.set_cos_readback(now, v as u16);
                }
            }
            MainDataSignal::SubStationOneState(v) => {
                if v == 0.0 || v == 1.0 {
                    self.auxiliary_off = v == 0.0;
                }
            }
            _ => {}
        }
        out
    }

    fn use_batteries(&mut self, now: Instant, out: &mut Vec<ModbusCommand>) {
        if self.grid.mode() != GridMode::Grid {
            return;
        }
        if self.mode != CompensationMode::Batteries {
            self.batteries.pause_regulation(now); // cancel shutdown, discard old Q
            self.generator.pause_regulation();
            self.mode = CompensationMode::Batteries;
        }
        self.batteries.set_allow_turn_on(true);
        if self.auxiliary_requested != Some(true) {
            out.push(self.auxiliary.on());
            self.auxiliary_requested = Some(true);
        }
    }

    pub fn grid_mode(&self) -> GridMode {
        self.grid.mode()
    }

    fn sync_grid(&mut self, now: Instant, out: &mut Vec<ModbusCommand>) {
        let mode = self.grid.mode();
        if mode != self.applied_grid {
            printers::warn(format!(
                "Режим мережі: {:?} -> {:?}",
                self.applied_grid, mode
            ));
            if mode == GridMode::Blackout {
                let (p_age, q_age) = self.grid.analyzer_ages(now);
                let age = |v: Option<std::time::Duration>| {
                    v.map(|d| format!("{:.1} с", d.as_secs_f64()))
                        .unwrap_or_else(|| "ще не отримано".into())
                };
                printers::warn(format!(
                    "Блекаут: немає P/Q аналізатора; останній P: {}, останній Q: {}, таймаут {:.1} с",
                    age(p_age),
                    age(q_age),
                    self.grid.cfg.analyzer_timeout.as_secs_f64()
                ));
            }
            self.applied_grid = mode;
            self.trend.clear();
            self.generator.pause_regulation();
            self.batteries.pause_regulation(now);
            self.last_off = None;
            if mode == GridMode::Grid {
                self.auxiliary_requested = None;
                if self.engine_run && self.gen_power.is_some_and(|p| p > 420.0) {
                    self.mode = CompensationMode::HandoffToGenerator;
                    self.batteries.set_allow_turn_on(false);
                    out.extend(self.batteries.start_soft_shutdown(now));
                } else {
                    self.mode = CompensationMode::Batteries;
                    self.use_batteries(now, out);
                }
            }
        }
        if mode != GridMode::Grid {
            self.batteries.set_allow_turn_on(false);
            let retry = self
                .last_off
                .is_none_or(|last| now.saturating_duration_since(last) >= self.grid.cfg.off_retry);
            if retry {
                let first = self.last_off.is_none();
                if first || self.relay_mask != Some(0) {
                    out.push(self.batteries.force_off(now));
                }
                if first || !self.auxiliary_off {
                    out.push(self.auxiliary.off());
                }
                self.auxiliary_requested = Some(false);
                self.last_off = Some(now);
            }
        }
    }

    pub fn callback(&mut self, now: Instant, cb: CommandCallback) {
        if self.remote.callback(cb) {
            return;
        }
        if matches!(cb, CommandCallback::Unsatisfied(_)) {
            printers::warn(format!("Команда координатора/актора не доставлена: {cb:?}"));
        }
        self.batteries.income_callback(now, cb);
        self.generator.income_callback(now, cb);
    }

    pub fn on_tick(&mut self, now: Instant) -> Vec<ModbusCommand> {
        let mut out = Vec::new();
        self.grid.tick(now);
        self.sync_grid(now, &mut out);
        out.extend(self.remote.on_tick(now));
        if self.grid.mode() != GridMode::Grid {
            return out;
        }
        out.extend(self.generator.on_tick(now));
        if self.mode != CompensationMode::Generator {
            out.extend(self.batteries.on_tick(now));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::condesators::{
        condensators_actor::{ActorConfig, Target},
        condensators_state::CondensatorState,
    };
    use super::super::generator::generator_actor::{GenTarget, GeneratorConfig};
    use super::*;
    use crate::meta::state::command::WriteCommand;
    use std::sync::{Arc, atomic::AtomicU64};

    fn make(now: Instant) -> HandoffCoordinator {
        let ids = Arc::new(AtomicU64::new(1));
        let target = Target {
            ip: Arc::new("127.0.0.1".into()),
            port: 502,
            unit: 1,
            register: 470,
            write: WriteCommand::Multi,
        };
        let batteries = CondensatorActor::new(
            CondensatorState::default(),
            ActorConfig::default(),
            target.clone(),
            ids.clone(),
            now,
        );
        let generator = GeneratorActor::new(
            GeneratorConfig::default(),
            GenTarget {
                ip: target.ip.clone(),
                port: 502,
                unit: 2,
                setpoint_register: 8,
                write: WriteCommand::Single,
            },
            ids.clone(),
            true,
        );
        let remote = RemoteActor::new(
            Target {
                register: 7,
                ..target.clone()
            },
            std::time::Duration::from_secs(5),
            ids.clone(),
        );
        let auxiliary = AuxiliaryStation {
            target: Target {
                register: 100,
                ..target
            },
            on_value: 1,
            off_value: 0,
            ids,
        };
        HandoffCoordinator::new(
            batteries,
            generator,
            remote,
            auxiliary,
            PowerTrendTracker::new(3, 2.0),
            GridMonitor::new(
                super::super::grid::GridConfig {
                    analyzer_timeout: std::time::Duration::from_secs(1000),
                    ..Default::default()
                },
                now,
            ),
        )
    }

    #[test]
    fn handoff_waits_for_actual_zero_then_stops_q_and_watchdog() {
        let now = Instant::now();
        let mut c = make(now);
        c.handle(now, MainDataSignal::CondensatorsRelayState(3.0));
        c.handle(now, MainDataSignal::GenEngineRun(1.0));
        for p in [400.0, 410.0, 420.0] {
            c.handle(now, MainDataSignal::GenActivePower(p));
        }
        assert_eq!(c.mode(), CompensationMode::Batteries); // strictly above 420
        let cmds = c.handle(now, MainDataSignal::GenActivePower(425.0));
        assert_eq!(c.mode(), CompensationMode::HandoffToGenerator);
        assert!(
            cmds.iter()
                .any(|v| v.state_register == 100 && v.new_state == vec![0])
        );
        assert!(
            cmds.iter()
                .any(|v| v.state_register == 470 && v.new_state == vec![2])
        );
        c.handle(now, MainDataSignal::CondensatorsRelayState(2.0));
        assert_eq!(c.mode(), CompensationMode::HandoffToGenerator);
        for _ in 0..3 {
            c.handle(now, MainDataSignal::MainQ(10.0));
        }
        c.handle(now, MainDataSignal::CondensatorsRelayState(0.0));
        assert_eq!(c.mode(), CompensationMode::Generator);
        for _ in 0..12 {
            c.handle(now, MainDataSignal::MainQ(90.0));
        }
        assert!(
            c.on_tick(now + std::time::Duration::from_secs(100))
                .is_empty()
        );
        assert_eq!(c.batteries.phase_name(), "Idle");
    }

    #[test]
    fn twenty_seconds_of_fresh_low_values_is_not_a_failsafe_condition() {
        let now = Instant::now();
        let mut c = make(now);
        c.handle(now, MainDataSignal::CondensatorsRelayState(0.0));
        c.handle(now, MainDataSignal::GenEngineRun(1.0));
        // Already in Generator mode, with no falling trend in the current window.
        c.mode = CompensationMode::Generator;
        for seconds in 0..=20 {
            let at = now + std::time::Duration::from_secs(seconds);
            assert!(c.handle(at, MainDataSignal::GenActivePower(5.0)).is_empty());
            assert!(c.handle(at, MainDataSignal::MainQ(5.0)).is_empty());
            assert!(c.on_tick(at).is_empty());
        }
        assert_eq!(c.mode(), CompensationMode::Generator);
    }

    #[test]
    fn island_forces_both_off_and_blocks_normal_handoff_until_grid_returns() {
        let now = Instant::now();
        let mut c = make(now);
        c.handle(now, MainDataSignal::CondensatorsRelayState(3.0));
        c.handle(now, MainDataSignal::GenEngineRun(1.0));
        let mut commands = Vec::new();
        for i in 0..=5 {
            let at = now + std::time::Duration::from_secs(i * 2);
            c.handle(at, MainDataSignal::MainP(15.0));
            commands.extend(c.handle(at, MainDataSignal::MainQ(0.0)));
        }
        assert_eq!(c.grid_mode(), GridMode::Island);
        assert!(
            commands
                .iter()
                .any(|c| c.state_register == 470 && c.new_state == vec![0])
        );
        assert!(
            commands
                .iter()
                .any(|c| c.state_register == 100 && c.new_state == vec![0])
        );
        let at = now + std::time::Duration::from_secs(11);
        assert!(
            c.handle(at, MainDataSignal::GenEngineRun(0.0))
                .iter()
                .all(|cmd| cmd.state_register != 470 && cmd.state_register != 100)
        );
        c.handle(at, MainDataSignal::MainP(100.0));
        c.handle(at, MainDataSignal::MainQ(50.0));
        assert_eq!(c.grid_mode(), GridMode::Grid);
        assert_eq!(c.mode(), CompensationMode::Batteries);
    }

    #[test]
    fn continuing_q_with_quiet_p_does_not_send_emergency_off() {
        let now = Instant::now();
        let mut c = make(now);
        c.grid = GridMonitor::new(Default::default(), now);
        c.handle(now, MainDataSignal::CondensatorsRelayState(0.0));
        c.handle(now, MainDataSignal::MainP(100.0));
        // No voltage provided: ordinary regulation will not request turn-on.
        for seconds in (0..=40).step_by(2) {
            let at = now + std::time::Duration::from_secs(seconds);
            assert!(c.handle(at, MainDataSignal::MainQ(170.0)).is_empty());
            assert!(c.on_tick(at).is_empty());
            assert_eq!(c.grid_mode(), GridMode::Grid);
        }
        let cmds = c.on_tick(now + std::time::Duration::from_secs(50));
        assert_eq!(c.grid_mode(), GridMode::Blackout);
        assert!(
            cmds.iter()
                .any(|c| c.state_register == 470 && c.new_state == vec![0])
        );
    }

    #[test]
    fn analyzer_silence_forces_both_off_retries_and_accepts_feedback() {
        let now = Instant::now();
        let mut c = make(now);
        c.grid = GridMonitor::new(Default::default(), now);
        let cmds = c.on_tick(now + std::time::Duration::from_secs(10));
        assert_eq!(c.grid_mode(), GridMode::Blackout);
        assert_eq!(cmds.len(), 2);
        let at = now + std::time::Duration::from_secs(15);
        assert_eq!(c.on_tick(at).len(), 2);
        c.handle(at, MainDataSignal::CondensatorsRelayState(0.0));
        c.handle(at, MainDataSignal::SubStationOneState(0.0));
        assert!(
            c.on_tick(now + std::time::Duration::from_secs(20))
                .is_empty()
        );
    }

    #[test]
    fn falling_power_returns_batteries_and_stop_cancels_shutdown() {
        let now = Instant::now();
        let mut c = make(now);
        c.handle(now, MainDataSignal::CondensatorsRelayState(3.0));
        c.handle(now, MainDataSignal::GenEngineRun(1.0));
        for p in [410.0, 420.0, 430.0] {
            c.handle(now, MainDataSignal::GenActivePower(p));
        }
        for p in [420.0, 410.0] {
            c.handle(now, MainDataSignal::GenActivePower(p));
        }
        assert_eq!(c.mode(), CompensationMode::HandoffToGenerator);
        let cmds = c.handle(now, MainDataSignal::GenActivePower(400.0));
        assert_eq!(c.mode(), CompensationMode::Batteries);
        assert!(
            cmds.iter()
                .any(|v| v.state_register == 100 && v.new_state == vec![1])
        );
        assert_eq!(c.batteries.phase_name(), "Idle");
        for p in [410.0, 420.0, 430.0] {
            c.handle(now, MainDataSignal::GenActivePower(p));
        }
        c.handle(now, MainDataSignal::GenEngineRun(0.0));
        assert_eq!(c.mode(), CompensationMode::Batteries);
        assert_eq!(c.batteries.phase_name(), "Idle");
    }
}
