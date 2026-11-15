//! Independent guardian of the KGU remote register. Only readback confirms 1.
use crate::{
    caluculation::condesators::condensators_actor::Target,
    logger::printers,
    meta::state::command::{CommandCallback, ModbusCommand},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub struct RemoteActor {
    target: Target,
    retry: Duration,
    ids: Arc<AtomicU64>,
    observed: Option<u16>,
    last_try: Option<Instant>,
    pending: Option<u64>,
}
impl RemoteActor {
    pub fn new(target: Target, retry: Duration, ids: Arc<AtomicU64>) -> Self {
        Self {
            target,
            retry,
            ids,
            observed: None,
            last_try: None,
            pending: None,
        }
    }
    pub fn set_readback(&mut self, now: Instant, value: u16) -> Option<ModbusCommand> {
        self.observed = Some(value);
        if value == 1 {
            self.pending = None;
        }
        self.on_tick(now)
    }
    pub fn on_tick(&mut self, now: Instant) -> Option<ModbusCommand> {
        // Do not invent an initial value before receiving any telemetry.
        if self.observed.is_none() || self.observed == Some(1) {
            return None;
        }
        if self
            .last_try
            .is_some_and(|last| now.saturating_duration_since(last) < self.retry)
        {
            return None;
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        self.pending = Some(id);
        self.last_try = Some(now);
        Some(ModbusCommand {
            id,
            state_register: self.target.register,
            new_state: vec![1],
            target_address: self.target.unit,
            target_ip: self.target.ip.clone(),
            target_port: self.target.port,
            write_command: self.target.write,
        })
    }
    /// True means this callback belongs to this actor.
    pub fn callback(&mut self, cb: CommandCallback) -> bool {
        let id = match cb {
            CommandCallback::Happy(id) | CommandCallback::Unsatisfied(id) => id,
        };
        if self.pending != Some(id) {
            return false;
        }
        if matches!(cb, CommandCallback::Unsatisfied(_)) {
            printers::warn(format!(
                "Remote КГУ: запис 1 не доставлено (id {id}), повторю за таймером"
            ));
        }
        // Happy confirms delivery, not the register value. Keep checking readback.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::state::command::WriteCommand;
    fn actor() -> RemoteActor {
        RemoteActor::new(
            Target {
                ip: Arc::new("127.0.0.1".into()),
                port: 502,
                unit: 1,
                register: 7,
                write: WriteCommand::Single,
            },
            Duration::from_secs(5),
            Arc::new(AtomicU64::new(1)),
        )
    }
    #[test]
    fn retries_without_new_telemetry_until_confirmed_one() {
        let now = Instant::now();
        let mut a = actor();
        assert!(a.on_tick(now).is_none());
        let first = a.set_readback(now, 0).unwrap();
        assert_eq!(first.state_register, 7);
        assert_eq!(first.new_state, vec![1]);
        assert!(a.callback(CommandCallback::Happy(first.id)));
        assert!(a.on_tick(now + Duration::from_secs(4)).is_none());
        let second = a.on_tick(now + Duration::from_secs(5)).unwrap();
        assert_ne!(first.id, second.id);
        a.set_readback(now + Duration::from_secs(6), 1);
        assert!(a.on_tick(now + Duration::from_secs(20)).is_none());
    }
    #[test]
    fn corrects_any_non_one_and_failure_does_not_disable_retries() {
        let now = Instant::now();
        let mut a = actor();
        assert!(a.set_readback(now, 1).is_none());
        let cmd = a.set_readback(now, 2).unwrap();
        assert!(a.callback(CommandCallback::Unsatisfied(cmd.id)));
        assert!(!a.callback(CommandCallback::Happy(cmd.id + 100)));
        assert!(a.on_tick(now + Duration::from_secs(5)).is_some());
    }
}
