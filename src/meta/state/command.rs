
use std::sync::Arc;

/// Результат доставки команди. Приходить від «поштаря» (з таймаутом) назад до актора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandCallback {
    /// Не доставлено / пристрій не відповів / відповів помилкою.
    Unsatisfied(u64),
    /// Пристрій прийняв запис.
    Happy(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCommand {
    /// 0x06: запис одного регістра.
    Single,
    /// 0x10: запис кількох регістрів. Деякі пристрої приймають і одиночний запис лише так.
    Multi,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModbusCommand {
    pub id: u64,
    pub state_register: u16,
    pub new_state: Vec<u16>,
    pub target_address: u16,
    pub target_ip: Arc<String>,
    pub target_port: u16,
    pub write_command: WriteCommand,
}
 