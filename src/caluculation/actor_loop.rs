use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use crate::caluculation::condesators::condensators_actor::{CondensatorActor, ActorConfig, Target};
use crate::caluculation::condesators::condensators_state::CondensatorState;
use tokio::time::{self, Duration};
use tokio::sync::mpsc;
use crate::meta::state::command::WriteCommand;
use crate::meta::state::main_data;
use crate::meta::state::main_data::MainDataSignal;
/*
шо у нас буде?
вольтаж, реактив на трансформаторі, стан реле коденсаторів, стан реле автономної конденсаторної установки(вкл-викл),
уставка косинусу генератора, потужність генератора, local-remote уставка косинусу генератора(вкл-викл, важливий ньюанс!),
стан самого генератора(вкл-викл)

нафіга нам вольтаж? хіба для перерахунку потужності конденсаторних батарей відносно вольтажу.

чим будемо керувати?
ввімкнення remote на ТРМ в КГУ, вкл-викл автономки, зміна стану реле конденсаторів, зміна уставки косинусу КГУ


TODO: додати сюди cross_scale::trim_setpoint, математика уставки косинусу
TODO: створити окремого актора, який буде відправляти команди пристроям, потрібно відслідковувати стан відправили-змінилось
TODO: створити енум вхідних подій для каналу цього актора від вебсокету і додати актора самого вебсокету(злизати з ТГ бота)
TODO: додати аналіз тенденції потужності генератора, придумати як розуміти, що напруги на трансформаторі немає(не приходять дані по Q) та режим острів
TODO: подумати про перерахування фактичної потужності батарей відносно вольтажу, але то смішні зміни
 */


// TODO переробити
async fn reactive_loop(mut rx: mpsc::Receiver<MainDataSignal>) {
    let ids_counter = Arc::new(AtomicU64::new(0));
    let coord_state = CondensatorState::load_or_create("../../config/condensators.toml").unwrap_or_else(|e| {
        println!("{:?}", &e);
        CondensatorState::default()
    });
    let mut main_actor = CondensatorActor::new(
        coord_state,
        ActorConfig::default(),
        Target { // TODO!!!!!!!
            ip: Arc::new(String::from("192.168.2.211")),
            port: 502,
            unit: 1,
            register: 470,
            write: WriteCommand::Multi
        },
        ids_counter,
        std::time::Instant::now()
    );

    let mut tick_interval = time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(event) => {
                        let now = std::time::Instant::now();

                    }
                    None => {
                        // приїхали...
                        break;
                    }
                }
            },
            _ = tick_interval.tick() => {
                let now = std::time::Instant::now();

            },
        }
    }
}