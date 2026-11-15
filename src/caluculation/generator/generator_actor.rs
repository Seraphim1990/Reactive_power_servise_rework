//! Актор КГУ: тримає Q мережі біля цілі уставкою cos (регістр 0..1000).
//! Чиста стан-машина без IO і без таймерів, у тому ж стилі, що й актор батарей.
//!
//! Публічний інтерфейс (час `now` завжди приходить ззовні):
//!   handle_q(now, Q)              - відлік Q мережі; може повернути новий запис уставки
//!   set_cos_readback(now, v)      - зчитана з пристрою уставка (джерело істини)
//!   set_active_power(now, P_gen)  - активна потужність КГУ
//!   set_remote(now, bool)         - дистанційний режим; при втраті повертає запис remote=1
//!   set_engine_run(now, bool)     - КГУ працює; при зупинці повертає запис «парковки» уставки
//!   income_callback(now, cb)      - результат доставки команди
//!   on_tick(now) / next_deadline()- таймаут кроку
//!
//! Два режими вибору уставки (прапорець `use_carriage` у `new`):
//!   true  - каретка: Q нижче коридору -> уставка +1, вище -> -1 (простий, повільний)
//!   false - перерахунок через потужність: одразу потрібна уставка (cos_scale::trim_setpoint)
//!
//! Логи: помилки й скасування -> printers::err, усе інше -> printers::event.
//! Сирі відліки Q і P_gen не логуються (шум раз на 2 с), логуються зміни стану й рішення.
//! Конвенція Q: + = споживання з мережі (недокомпенсація). Більше уставка -> КГУ видає менше Q.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::cos_scale::{carriage_step, park_value, trim_setpoint, TrimParams};
use super::super::condesators::ring::RingBuf;
use crate::logger::printers;
use crate::meta::state::command::{CommandCallback, ModbusCommand, WriteCommand};

/// Вікно усереднення Q для рішення (3 відліки = 6 с при опитуванні 2 с).
const GEN_WINDOW: usize = 3;

// ───────────────────────── Налаштування ─────────────────────────

/// Кого актор контролює.
#[derive(Debug, Clone)]
pub struct GenTarget {
    pub ip: Arc<String>,
    pub port: u16,
    pub unit: u16,              // адреса пристрою
    pub setpoint_register: u16, // регістр уставки cos (8)
    pub remote_register: u16,   // регістр дистанційного режиму (7)
    pub write: WriteCommand,
}

#[derive(Debug, Clone)]
pub struct GeneratorConfig {
    pub trim: TrimParams,         // ціль, коридор, захисна зона навколо 800, ліміт кроку
    pub p_min_listen_kw: f32,     // нижче цієї P_gen КГУ уставку ігнорує
    pub carriage_step_units: i32, // крок каретки, одиниць уставки
    pub step_timeout: Duration,   // скільки чекати, поки зчитана уставка збіжиться
    pub settle_skip: u8,          // скільки перших Q після запису відкинути
    pub remote_retry: Duration,   // мінімальна пауза між спробами повернути дистанційний режим
}

impl Default for GeneratorConfig {
    fn default() -> Self {
        Self {
            trim: TrimParams::default(),
            p_min_listen_kw: 400.0,
            carriage_step_units: 1,
            step_timeout: Duration::from_secs(8),
            settle_skip: 1,
            remote_retry: Duration::from_secs(5),
        }
    }
}

// ───────────────────────── Фази ─────────────────────────

#[derive(Debug)]
enum Phase {
    /// Збираємо Q і приймаємо рішення.
    Idle,
    /// Записали уставку `want`, чекаємо, поки зчитана уставка стане такою ж.
    /// acked = пристрій прийняв запис; bad_reads = скільки зчитувань поспіль не збіглись після цього.
    Sending { want: u16, id: u64, acked: bool, bad_reads: u8, deadline: Instant },
    /// Після запису: відкидаємо `skip` перших Q, збираємо ще `need`.
    Settling { skip: u8, need: u8 },
}

// ───────────────────────── Актор ─────────────────────────

pub struct GeneratorActor {
    cfg: GeneratorConfig,
    target: GenTarget,
    id_counter: Arc<AtomicU64>,
    use_carriage: bool,

    cos_now: Option<u16>, // зчитана уставка
    p_gen: Option<f32>,
    remote: bool,
    engine_run: bool,
    was_regulating: bool, // для логу переходів «регулюю / не регулюю»
    last_remote_try: Option<Instant>,

    q_buf: RingBuf<GEN_WINDOW>,
    phase: Phase,
    last_alarm: Option<String>,
}

impl GeneratorActor {
    pub fn new(
        cfg: GeneratorConfig,
        target: GenTarget,
        id_counter: Arc<AtomicU64>,
        use_carriage: bool,
    ) -> Self {
        let actor = Self {
            cfg,
            target,
            id_counter,
            use_carriage,
            cos_now: None,
            p_gen: None,
            remote: false,
            engine_run: false,
            was_regulating: false,
            last_remote_try: None,
            q_buf: RingBuf::new(),
            phase: Phase::Idle,
            last_alarm: None,
        };
        actor.log(format!(
            "актор створено: режим {}, коридор {:.1}..{:.1} квар, КГУ слухає уставку від {:.0} кВт, пристрій {}:{} (unit {}, уставка в регістрі {}, дистанційний режим в {})",
            actor.mode_name(),
            actor.cfg.trim.target_kvar - actor.cfg.trim.deadband_floor_kvar,
            actor.cfg.trim.target_kvar + actor.cfg.trim.deadband_floor_kvar,
            actor.cfg.p_min_listen_kw,
            actor.target.ip,
            actor.target.port,
            actor.target.unit,
            actor.target.setpoint_register,
            actor.target.remote_register
        ));
        actor
    }

    // ───────────── Публічний інтерфейс ─────────────

    /// Відлік Q мережі. Якщо час приймати рішення, повертає запис нової уставки.
    pub fn handle_q(&mut self, now: Instant, q: f64) -> Option<ModbusCommand> {
        let q = q as f32;
        if !q.is_finite() {
            self.alarm(format!("некоректне Q: {q}, ігнорую"));
            return None;
        }

        // Переходи «регулюю / не регулюю» логуємо один раз, з причиною
        let reg = self.regulating();
        if reg != self.was_regulating {
            self.was_regulating = reg;
            if reg {
                self.log(format!(
                    "регулювання ввімкнено: КГУ працює, дистанційний режим є, P_gen {:.0} кВт",
                    self.p_gen.unwrap_or(0.0)
                ));
            } else {
                self.log(format!("регулювання призупинено: {}", self.why_not_regulating()));
            }
        }

        // КГУ зараз уставку не слухає: нічого не накопичуємо (інакше інтегратор «накручується»)
        if !reg {
            self.q_buf.clear();
            self.phase = Phase::Idle;
            return None;
        }

        if matches!(self.phase, Phase::Idle) {
            self.q_buf.push(q);
            return self.decide(now);
        }

        // Settling (у Sending відліки не потрібні: буфер чиститься після запису)
        let mut finished = false;
        if let Phase::Settling { skip, need } = &mut self.phase {
            if *skip > 0 {
                *skip -= 1;
            } else {
                self.q_buf.push(q);
                *need = need.saturating_sub(1);
                if *need == 0 {
                    finished = true;
                }
            }
        }
        if finished {
            self.log("стабілізація завершена, повертаюсь до рішень".to_string());
            self.phase = Phase::Idle;
            return self.decide(now);
        }
        None
    }

    /// Зчитана з пристрою уставка. Джерело істини: збіг з відправленою завершує крок.
    pub fn set_cos_readback(&mut self, now: Instant, v: u16) {
        if self.cos_now != Some(v) {
            match self.cos_now {
                Some(old) => self.log(format!("уставка на пристрої: {old} -> {v}")),
                None => self.log(format!("уставка на пристрої: {v} (перше зчитування)")),
            }
        }
        self.cos_now = Some(v);

        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Sending { want, id, acked, bad_reads, deadline } = phase {
            if v == want {
                self.log(format!("уставка {v} збіглась з відправленою, крок завершено"));
                self.enter_settling();
                return;
            }
            if now >= deadline {
                self.alarm(format!(
                    "уставка КГУ не збіглась вчасно (очікувалась {want}, зчитано {v}), крок скасовано"
                ));
                self.enter_settling();
                return;
            }
            // Розбіжність має вагу лише після підтвердження пристрою, і лише двічі поспіль
            let mut bad_reads = bad_reads;
            if acked {
                bad_reads += 1;
                if bad_reads >= 2 {
                    self.alarm(format!(
                        "КГУ підтвердив запис, але уставка не змінилась (очікувалась {want}, зчитано {v}), крок скасовано"
                    ));
                    self.enter_settling();
                    return;
                }
            }
            self.log(format!(
                "очікую уставку {want}, зчитано {v} (розбіжних зчитувань після квитанції: {bad_reads})"
            ));
            self.phase = Phase::Sending { want, id, acked, bad_reads, deadline };
            return;
        }
        self.phase = phase;
    }

    pub fn set_active_power(&mut self, _now: Instant, p_kw: f64) {
        let p = p_kw as f32;
        if p.is_finite() {
            self.p_gen = Some(p);
        } else {
            self.alarm(format!("некоректна потужність КГУ: {p_kw}, ігнорую"));
        }
    }

    /// Дистанційний режим. Регістри пристрою інколи злітають у дефолт, тому при втраті
    /// режиму повертаємо запис remote=1, але не частіше ніж раз на `remote_retry`.
    pub fn set_remote(&mut self, now: Instant, remote: bool) -> Option<ModbusCommand> {
        if remote != self.remote {
            if remote {
                self.log("дистанційний режим КГУ є".to_string());
            } else {
                self.alarm("дистанційний режим КГУ втрачено (регістри могли злетіти в дефолт)");
            }
        }
        self.remote = remote;
        if remote {
            return None;
        }
        if let Some(last) = self.last_remote_try {
            if now.saturating_duration_since(last) < self.cfg.remote_retry {
                return None;
            }
        }
        self.last_remote_try = Some(now);
        let cmd = self.make_command(self.target.remote_register, 1);
        self.log(format!(
            "повертаю дистанційний режим: пишу 1 в регістр {} (id {})",
            self.target.remote_register, cmd.id
        ));
        Some(cmd)
    }

    /// КГУ працює / зупинений. При зупинці паркуємо уставку (795), щоб наступний старт
    /// починався з відомого значення без переходу через одиницю.
    pub fn set_engine_run(&mut self, _now: Instant, on: bool) -> Option<ModbusCommand> {
        let was = self.engine_run;
        self.engine_run = on;
        if was == on {
            return None;
        }
        self.q_buf.clear();
        self.phase = Phase::Idle;
        if on {
            self.log("КГУ запущено".to_string());
            return None;
        }
        self.log("КГУ зупинено".to_string());
        let park = park_value(self.cfg.trim.guard_units) as u16;
        if self.cos_now == Some(park) {
            self.log(format!("уставка вже {park}, паркування не потрібне"));
            return None;
        }
        let cmd = self.make_command(self.target.setpoint_register, park);
        self.log(format!(
            "паркую уставку: пишу {} в регістр {} (id {})",
            park, self.target.setpoint_register, cmd.id
        ));
        Some(cmd)
    }

    /// Результат доставки команди. Відповіді з чужим (старим) id ігноруються.
    pub fn income_callback(&mut self, _now: Instant, cb: CommandCallback) {
        let mut ours_acked = false;
        let mut ours_failed = false;
        if let Phase::Sending { id, acked, .. } = &mut self.phase {
            match cb {
                CommandCallback::Happy(cid) if cid == *id => {
                    *acked = true;
                    ours_acked = true;
                }
                CommandCallback::Unsatisfied(cid) if cid == *id => {
                    ours_failed = true;
                }
                _ => {}
            }
        }

        match cb {
            CommandCallback::Happy(cid) => {
                if ours_acked {
                    self.log(format!("квитанція: пристрій прийняв запис уставки (id {cid})"));
                } else {
                    self.log(format!("квитанція id {cid} не стосується поточного кроку (віддалений режим, паркування чи старий крок)"));
                }
            }
            CommandCallback::Unsatisfied(cid) => {
                if ours_failed {
                    self.alarm(format!("команду уставки id {cid} не доставлено, крок скасовано"));
                    self.enter_settling();
                } else {
                    self.alarm(format!("команду id {cid} не доставлено (не поточний крок)"));
                }
            }
        }
    }

    /// Таймаут кроку. Викликати по тіку або в момент `next_deadline()`.
    pub fn on_tick(&mut self, now: Instant) -> Option<ModbusCommand> {
        let timed_out = match &self.phase {
            Phase::Sending { deadline, .. } => now >= *deadline,
            _ => false,
        };
        if timed_out {
            self.alarm("уставка КГУ не збіглась вчасно, крок скасовано");
            self.enter_settling();
        }
        None
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match &self.phase {
            Phase::Sending { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }

    pub fn pause_regulation(&mut self) {
        self.phase = Phase::Idle;
        self.q_buf.clear();
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "Idle",
            Phase::Sending { .. } => "Sending",
            Phase::Settling { .. } => "Settling",
        }
    }

    pub fn last_alarm(&self) -> Option<&str> {
        self.last_alarm.as_deref()
    }

    // ───────────── Внутрішнє ─────────────

    fn log(&self, msg: String) {
        printers::event(format!("КГУ: {msg}"));
    }

    fn alarm(&mut self, msg: impl Into<String>) {
        let msg: String = msg.into();
        printers::err(format!("КГУ: {msg}"));
        self.last_alarm = Some(msg);
    }

    fn mode_name(&self) -> &'static str {
        if self.use_carriage {
            "каретка"
        } else {
            "перерахунок через потужність"
        }
    }

    /// Чи слухає КГУ зараз уставку: працює, дистанційний режим, потужність вище мертвої зони.
    fn regulating(&self) -> bool {
        if !self.engine_run || !self.remote {
            return false;
        }
        match self.p_gen {
            Some(p) => p >= self.cfg.p_min_listen_kw,
            None => false,
        }
    }

    fn why_not_regulating(&self) -> String {
        if !self.engine_run {
            return "КГУ не працює".to_string();
        }
        if !self.remote {
            return "немає дистанційного режиму".to_string();
        }
        match self.p_gen {
            Some(p) => format!(
                "P_gen {:.0} кВт нижче порога {:.0} кВт (КГУ уставку ігнорує)",
                p, self.cfg.p_min_listen_kw
            ),
            None => "P_gen невідома".to_string(),
        }
    }

    fn make_command(&self, register: u16, value: u16) -> ModbusCommand {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        ModbusCommand {
            id,
            state_register: register,
            new_state: vec![value],
            target_address: self.target.unit,
            target_ip: Arc::clone(&self.target.ip),
            target_port: self.target.port,
            write_command: self.target.write,
        }
    }

    fn enter_settling(&mut self) {
        self.q_buf.clear();
        self.phase = Phase::Settling { skip: self.cfg.settle_skip, need: GEN_WINDOW as u8 };
        self.log(format!(
            "стабілізація: відкидаю {} відлік(ів) Q, збираю {}",
            self.cfg.settle_skip, GEN_WINDOW
        ));
    }

    fn decide(&mut self, now: Instant) -> Option<ModbusCommand> {
        let q = self.q_buf.mean_last(GEN_WINDOW)?; // вікно ще не повне -> None
        let v_now = self.cos_now? as i32; // не знаємо уставки пристрою -> не рахуємо
        let p_gen = self.p_gen?;

        let v_new = if self.use_carriage {
            carriage_step(q, v_now, &self.cfg.trim, self.cfg.carriage_step_units)?
        } else {
            trim_setpoint(p_gen, v_now, q, &self.cfg.trim)?
        };

        let want = v_new as u16;
        let cmd = self.make_command(self.target.setpoint_register, want);
        self.log(format!(
            "рішення ({}): Q = {:.1} квар, P_gen {:.0} кВт, уставка {} -> {}, пишу в регістр {} (id {})",
            self.mode_name(),
            q,
            p_gen,
            v_now,
            want,
            self.target.setpoint_register,
            cmd.id
        ));
        self.phase = Phase::Sending {
            want,
            id: cmd.id,
            acked: false,
            bad_reads: 0,
            deadline: now + self.cfg.step_timeout,
        };
        Some(cmd)
    }
}