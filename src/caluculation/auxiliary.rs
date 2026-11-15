//! Placeholder: configure the real address/register and device values before use.
use super::condesators::condensators_actor::Target;
use crate::meta::state::command::ModbusCommand;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub struct AuxiliaryStation {
    pub target: Target,
    pub on_value: u16,
    pub off_value: u16,
    pub ids: Arc<AtomicU64>,
}

impl AuxiliaryStation {
    pub fn on(&self) -> ModbusCommand {
        self.command(self.on_value)
    }
    pub fn off(&self) -> ModbusCommand {
        self.command(self.off_value)
    }
    fn command(&self, value: u16) -> ModbusCommand {
        ModbusCommand {
            id: self.ids.fetch_add(1, Ordering::Relaxed),
            state_register: self.target.register,
            new_state: vec![value],
            target_address: self.target.unit,
            target_ip: Arc::clone(&self.target.ip),
            target_port: self.target.port,
            write_command: self.target.write,
        }
    }
}
