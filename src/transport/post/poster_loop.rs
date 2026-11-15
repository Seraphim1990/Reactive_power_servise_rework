use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_modbus::client::{tcp, Context};
use tokio_modbus::{Error, ExceptionCode, ProtocolError, Slave};
use tokio_modbus::prelude::*;
use crate::logger::printers;
// use crate::meta::command::{CommandCallback, ModbusCommand, WriteCommand};
use crate::meta::state::command::*;
#[derive(Debug, Clone, Copy)]
enum ModbusWriteError {
    Timeout,
    DeviceProtocolError,
    SocketFail,
    HeaderMisMatch,
}

type ModbusWriteResult = Result<(), ModbusWriteError>;

pub struct WriteMaster {
    contexts: HashMap<Arc<String>, Context>,
}

impl WriteMaster {
    pub fn new() -> Self {
        Self {
            contexts: HashMap::new(),
        }
    }

    pub async fn run(
        &mut self,
        mut rx_commands: mpsc::Receiver<(ModbusCommand, oneshot::Sender<CommandCallback>)>,
    ) {
        // printers::event(String::from("Старт актора запису (Write Master)"));

        while let Some((cmd, respond_to)) = rx_commands.recv().await {
            let ip = cmd.target_ip.clone();
            let port = cmd.target_port;

            // Перевіряємо наявність контексту в хеші, якщо не існує — створюємо
            let ctx = match self.contexts.get_mut(&ip) {
                Some(ctx) => ctx,
                None => {
                    match Self::create_context(&ip, port).await {
                        Some(new_ctx) => {
                            self.contexts.insert(ip.clone(), new_ctx);
                            self.contexts.get_mut(&ip).unwrap()
                        }
                        None => {
                            let _ = respond_to.send(CommandCallback::Unsatisfied(cmd.id));
                            continue;
                        }
                    }
                }
            };

            let slave = Slave(cmd.target_address as u8);
            ctx.set_slave(slave);

            let timeout_duration = 300; // Можна налаштувати за потреби

            let write_res = match cmd.write_command {
                WriteCommand::Single => {
                    if cmd.new_state.is_empty() {
                        let _ = respond_to.send(CommandCallback::Unsatisfied(cmd.id));
                        continue;
                    }
                    let val = cmd.new_state[0];
                    write_process(
                        || ctx.write_single_register(cmd.state_register, val),
                        timeout_duration,
                        cmd.id,
                        ip.clone(),
                    )
                        .await
                }
                WriteCommand::Multi => {
                    write_process(
                        || ctx.write_multiple_registers(cmd.state_register, &cmd.new_state),
                        timeout_duration,
                        cmd.id,
                        ip.clone(),
                    )
                        .await
                }
            };

            let callback = match write_res {
                Ok(()) => CommandCallback::Happy(cmd.id),
                Err(e) => {
                    // Логіка перепідключення: при пошкодженні сокета або заголовка видаляємо контекст з хешу
                    match e {
                        ModbusWriteError::SocketFail | ModbusWriteError::HeaderMisMatch => {
                            printers::err(format!(
                                "Закрито з'єднання або помилка заголовка для ip: {}, видаляємо контекст",
                                ip
                            ));
                            self.contexts.remove(&ip);
                        }
                        _ => {}
                    }
                    CommandCallback::Unsatisfied(cmd.id)
                }
            };

            let _ = respond_to.send(callback);
        }

        printers::warn(String::from("Канал команд для Write Master закрився"));
    }

    async fn create_context(ip: &str, port: u16) -> Option<Context> {
        let socket_addr = match format!("{}:{}", ip, port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                printers::err(format!("Невірна адреса сокету для запису {}: {}", ip, e));
                return None;
            }
        };

        printers::event(format!("Підключення Write Master до ip: {}:{}", ip, port));
        match tcp::connect(socket_addr).await {
            Ok(ctx) => {
                printers::event(format!("Контекст запису успішно створено для ip: {}", ip));
                Some(ctx)
            }
            Err(e) => {
                printers::err(format!(
                    "Помилка створення контексту для ip: {}, \nerr: {} \nПовторна спроба при наступній команді.",
                    ip, e
                ));
                None
            }
        }
    }
}

async fn write_process<F, Fut>(
    f: F,
    timeout_duration: u64,
    device_id: u64,
    node_ip: Arc<String>,
) -> ModbusWriteResult
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Result<(), ExceptionCode>, Error>>,
{
    let timeout_duration = Duration::from_millis(timeout_duration);
    let res = timeout(timeout_duration, f()).await;
    match res {
        Ok(Ok(data)) => match data {
            Ok(()) => Ok(()),
            Err(e) => match e {
                ExceptionCode::IllegalFunction => {
                    printers::err(format!(
                        "Пристрій не підтримує функцію запиту\n    id: {} нода {}",
                        device_id, node_ip
                    ));
                    Err(ModbusWriteError::DeviceProtocolError)
                }
                ExceptionCode::IllegalDataAddress => {
                    printers::err(format!(
                        "Запитані регістри для запису не існують\n    id: {} нода {}",
                        device_id, node_ip
                    ));
                    Err(ModbusWriteError::DeviceProtocolError)
                }
                ExceptionCode::IllegalDataValue | ExceptionCode::MemoryParityError => {
                    printers::err(format!(
                        "Біда з даними/паритетом при записі (CRC?)\n    id: {} нода {}",
                        device_id, node_ip
                    ));
                    Err(ModbusWriteError::Timeout)
                }
                ExceptionCode::ServerDeviceBusy | ExceptionCode::Acknowledge => {
                    Err(ModbusWriteError::Timeout)
                }
                ExceptionCode::ServerDeviceFailure
                | ExceptionCode::GatewayPathUnavailable
                | ExceptionCode::GatewayTargetDevice => {
                    Err(ModbusWriteError::SocketFail)
                }
                _ => Err(ModbusWriteError::Timeout),
            },
        },
        Ok(Err(e)) => match e {
            Error::Transport(io_err) => match io_err.kind() {
                ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionReset
                | ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::ConnectionAborted
                | ErrorKind::NotConnected
                | ErrorKind::BrokenPipe
                | ErrorKind::TimedOut => Err(ModbusWriteError::SocketFail),
                ErrorKind::NetworkDown => {
                    printers::err(String::from("Упала мережа системи при записі"));
                    Err(ModbusWriteError::SocketFail)
                }
                other => {
                    printers::warn(format!("Дивна помилка IO при записі ModbusTCP:\n {:?}", other));
                    Err(ModbusWriteError::Timeout)
                }
            },
            Error::Protocol(protocol_error) => match protocol_error {
                ProtocolError::FunctionCodeMismatch { .. } => {
                    printers::warn(format!(
                        "Код відповіді не відповідає запиту при записі\n id: {}, для ноди {}",
                        device_id, node_ip
                    ));
                    Err(ModbusWriteError::DeviceProtocolError)
                }
                ProtocolError::HeaderMismatch { message, .. } => {
                    printers::warn(format!(
                        "Заголовок відповіді не відповідає запиту при записі: {}\n id: {}, для ноди {}",
                        message, device_id, node_ip
                    ));
                    Err(ModbusWriteError::HeaderMisMatch)
                }
            },
        },
        Err(_) => Err(ModbusWriteError::Timeout),
    }
}