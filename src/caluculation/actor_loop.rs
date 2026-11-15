//! API adapter sends MainDataSignal here; WriteMaster runs independently.
use super::handoff::HandoffCoordinator;
use crate::meta::state::{
    command::{CommandCallback, ModbusCommand},
    main_data::MainDataSignal,
};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{self, Duration};

pub type WriteRequest = (ModbusCommand, oneshot::Sender<CommandCallback>);

pub async fn reactive_loop(
    mut rx: mpsc::UnboundedReceiver<MainDataSignal>,
    tx_write: mpsc::Sender<WriteRequest>,
    mut coordinator: HandoffCoordinator,
) {
    let mut ticks = time::interval(Duration::from_millis(250));
    ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut deliveries = JoinSet::new();
    loop {
        let commands = tokio::select! {
            event = rx.recv() => match event {
                Some(event) => coordinator.handle(Instant::now(), event),
                None => break,
            },
            _ = ticks.tick() => coordinator.on_tick(Instant::now()),
            result = deliveries.join_next(), if !deliveries.is_empty() => {
                if let Some(Ok(cb)) = result {
                    coordinator.callback(Instant::now(), cb);
                }
                Vec::new()
            }
        };
        for cmd in commands {
            let id = cmd.id;
            let (reply, rx_reply) = oneshot::channel();
            // Enqueue in decision order: do not race independent send tasks.
            // A full/closed queue reports failure without blocking Q or timers.
            if tx_write.try_send((cmd, reply)).is_err() {
                coordinator.callback(Instant::now(), CommandCallback::Unsatisfied(id));
                continue;
            }
            deliveries.spawn(async move {
                match time::timeout(Duration::from_secs(8), rx_reply).await {
                    Ok(Ok(cb)) => cb,
                    _ => CommandCallback::Unsatisfied(id),
                }
            });
        }
    }
    deliveries.abort_all();
}
