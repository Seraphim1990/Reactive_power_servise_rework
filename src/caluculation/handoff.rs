//! Routes telemetry to the actors; returned commands must go to WriteMaster.
//! API tag names are deliberately outside this module: map them to MainDataSignal.
use super::condesators::condensators_actor::CondensatorActor;
use super::generator::generator_actor::GeneratorActor;
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
    auxiliary: AuxiliaryStation,
    trend: PowerTrendTracker,
    mode: CompensationMode,
    engine_run: bool,
    auxiliary_requested: Option<bool>,
}

impl HandoffCoordinator {
    pub fn new(
        batteries: CondensatorActor,
        generator: GeneratorActor,
        auxiliary: AuxiliaryStation,
        trend: PowerTrendTracker,
    ) -> Self {
        Self {
            batteries,
            generator,
            auxiliary,
            trend,
            mode: CompensationMode::Batteries,
            engine_run: false,
            auxiliary_requested: None,
        }
    }

    pub fn mode(&self) -> CompensationMode {
        self.mode
    }

    pub fn handle(&mut self, now: Instant, event: MainDataSignal) -> Vec<ModbusCommand> {
        let mut out = Vec::new();
        match event {
            MainDataSignal::MiddleVoltage(v) => self.batteries.set_voltage(now, v),
            MainDataSignal::MainQ(q) => {
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
                out.extend(self.batteries.set_mask(now, v as u16));
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
                self.generator.set_active_power(now, p);
                if !self.engine_run {
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
            MainDataSignal::GenSetterLocalRemoteState(v) if v == 0.0 || v == 1.0 => {
                out.extend(self.generator.set_remote(now, v == 1.0));
            }
            MainDataSignal::GenCosSetpointOutput(v) => {
                if v.is_finite() && (0.0..=1000.0).contains(&v) && v.fract() == 0.0 {
                    self.generator.set_cos_readback(now, v as u16);
                }
            }
            MainDataSignal::SubStationOneState(_) => {
                // Placeholder only: feedback/retries depend on the real device.
            }
            _ => {}
        }
        out
    }

    fn use_batteries(&mut self, now: Instant, out: &mut Vec<ModbusCommand>) {
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

    pub fn callback(&mut self, now: Instant, cb: CommandCallback) {
        if matches!(cb, CommandCallback::Unsatisfied(_)) {
            printers::warn(format!("Команда координатора/актора не доставлена: {cb:?}"));
        }
        self.batteries.income_callback(now, cb);
        self.generator.income_callback(now, cb);
    }

    pub fn on_tick(&mut self, now: Instant) -> Vec<ModbusCommand> {
        let mut out = Vec::new();
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
                remote_register: 7,
                write: WriteCommand::Single,
            },
            ids.clone(),
            true,
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
            auxiliary,
            PowerTrendTracker::new(3, 2.0),
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
