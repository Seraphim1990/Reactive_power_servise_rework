//! Актор конденсаторних батарей: чиста стан-машина без IO і без таймерів.
//!
//! Публічний інтерфейс (час `now` завжди приходить ззовні, тому все тестується):
//!   set_voltage(now, V10кВ)   - оновлює поправку ємності на V²
//!   set_mask(now, маска)      - фактичний стан реле (джерело істини); може повернути наступний крок
//!   handle_q(now, Q)          - відлік Q; може повернути перший крок нового плану
//!   income_callback(now, cb)  - результат доставки команди від «поштаря»
//!   on_tick(now)              - таймаути й Failsafe (викликати по тіку або по next_deadline())
//!   next_deadline()           - коли актору потрібно прокинутись, якщо нічого не прийде
//!   set_kgu(on) / set_allow_turn_on(bool) - режим
//!   start_soft_shutdown(now)  - м'який перехід: вимикає всі щаблі по одному, між кроками
//!                               чекає стабілізації Q (щоб КГУ встиг підхопити). Викликати
//!                               разом із set_allow_turn_on(false).
//!
//! Команди повертаються як `ModbusCommand` з унікальним id (спільний AtomicU64).
//! Логи: помилки й скасування -> printers::err, усе інше (зміни стану, рішення, команди,
//! квитанції) -> printers::event. Сирі відліки Q і напруги не логуються (шум раз на 2 с).
//! Конвенція Q: + = споживання з мережі (недокомпенсація).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::condensators_state::CondensatorState;
use super::planner::{current_mask, plan, shutdown_steps, Bank, Input, Params};
use super::ring::RingBuf;
use crate::logger::printers;
use crate::meta::state::command::{CommandCallback, ModbusCommand, WriteCommand};

/// Вікно повільного шляху: 10 відліків (20 с при опитуванні 2 с).
const SLOW_WINDOW: usize = 10;

// ───────────────────────── Допоміжне ─────────────────────────

/// 0b111 замість 0b0000000000000111.
fn bits(mask: u16) -> String {
    format!("{:#b}", mask)
}

fn steps_str(steps: &[u16]) -> String {
    let mut s = String::new();
    for i in 0..steps.len() {
        if i > 0 {
            s.push_str(" -> ");
        }
        s.push_str(&bits(steps[i]));
    }
    s
}

// ───────────────────────── Налаштування ─────────────────────────

/// Кого актор контролює: все, що воркер має знати про свій пристрій.
#[derive(Debug, Clone)]
pub struct Target {
    pub ip: Arc<String>,
    pub port: u16,
    pub unit: u16,     // адреса пристрою (-> ModbusCommand::target_address)
    pub register: u16, // регістр реле, 470 (-> ModbusCommand::state_register)
    pub write: WriteCommand,
}

/// Таймінги актора. Поки значення за замовчуванням; потім виносяться у файл.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ActorConfig {
    #[serde(with = "crate::caluculation::settings::seconds")]
    pub q_stale_island: Duration,   // немає Q і КГУ був ввімкнений -> острів
    #[serde(with = "crate::caluculation::settings::seconds")]
    pub q_stale_grid: Duration,     // немає Q і КГУ був вимкнений -> аварія зв'язку
    #[serde(with = "crate::caluculation::settings::seconds")]
    pub step_timeout: Duration,     // скільки чекати виконання кроку плану
    pub settle_skip: u8,            // скільки перших Q після плану відкинути
    pub settle_samples: u8,         // скільки свіжих Q зібрати перед наступним рішенням
    #[serde(with = "crate::caluculation::settings::seconds")]
    pub failsafe_step: Duration,    // пауза між гасінням щаблів у Failsafe
    pub failsafe_exit_samples: u32, // скільки свіжих Q потрібно, щоб вийти з Failsafe
    pub shed_q_tolerance_steps: f32, // м'яке вимкнення: наступний крок, коли Q <= ціль + стільки найменших щаблів
    #[serde(with = "crate::caluculation::settings::seconds")]
    pub shed_timeout: Duration,      // м'яке вимкнення: скільки чекати стабілізації Q перед наступним кроком
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            q_stale_island: Duration::from_secs(20),
            q_stale_grid: Duration::from_secs(60),
            step_timeout: Duration::from_secs(8),
            settle_skip: 1,
            settle_samples: 2,
            failsafe_step: Duration::from_secs(1),
            failsafe_exit_samples: 5,
            shed_q_tolerance_steps: 2.0,
            shed_timeout: Duration::from_secs(60),
        }
    }
}

// ───────────────────────── Фази ─────────────────────────

#[derive(Debug)]
enum Phase {
    /// Збираємо Q і приймаємо рішення.
    Idle,
    /// Виконуємо план. Крок `idx` відправлено (id), чекаємо підтвердження ЗА МАСКОЮ.
    /// acked = пристрій прийняв запис; bad_reads = скільки опитаних масок поспіль
    /// не збіглись з очікуваною ПІСЛЯ підтвердження.
    Sending {
        steps: Vec<u16>,
        idx: usize,
        prev: u16,
        id: u64,
        acked: bool,
        bad_reads: u8,
        deadline: Instant,
        shed: bool, // м'яке вимкнення: після підтвердження чекаємо стабілізації Q
    },
    /// М'яке вимкнення: крок `next - 1` виконано, чекаємо, поки Q стабілізується (КГУ підхопив),
    /// і лише тоді відправляємо крок `next`. `deadline` - скільки максимум чекати.
    Handoff { steps: Vec<u16>, next: usize, prev: u16, skip: u8, deadline: Instant },
    /// План закінчено: відкидаємо `skip` перших Q, збираємо ще `need`.
    Settling { skip: u8, need: u8 },
    /// Втрата даних: гасимо щаблі по одному (next_at - коли можна гасити наступний).
    Failsafe { mask: u16, to_off: Vec<usize>, fresh: u32, next_at: Instant },
}

// ───────────────────────── Актор ─────────────────────────

pub struct CondensatorActor {
    state: CondensatorState,
    params: Params,
    cfg: ActorConfig,
    target: Target,
    id_counter: Arc<AtomicU64>,

    banks: Vec<Bank>,
    have_relays: bool, // чи вже бачили хоч одну маску реле (до цього банки не плануємо)

    q_buf: RingBuf<SLOW_WINDOW>,
    last_q_at: Instant,
    last_v10: Option<f32>,
    kgu_on: bool,        // останній відомий стан КГУ
    allow_turn_on: bool, // дозвіл вмикати (згодом замінить Mode)

    phase: Phase,
    last_alarm: Option<String>,
}

impl CondensatorActor {
    pub fn new(
        state: CondensatorState,
        cfg: ActorConfig,
        target: Target,
        id_counter: Arc<AtomicU64>,
        now: Instant,
    ) -> Self {
        let params = Self::make_params(&state);

        // Старт: усі щаблі вважаємо вимкненими й ставимо повне ембарго на вмикання.
        // Перезапуск програми не повинен обходити розряд конденсаторів.
        let mut banks: Vec<Bank> = Vec::new();
        for i in 0..state.banks_kvar.len() {
            banks.push(Bank {
                c_eff: state.banks_kvar[i],
                on: false,
                no_on_before: now + state.effective_min_off(i),
                no_off_before: now,
            });
        }

        let actor = Self {
            state,
            params,
            cfg,
            target,
            id_counter,
            banks,
            have_relays: false,
            q_buf: RingBuf::new(),
            last_q_at: now,
            last_v10: None,
            kgu_on: false,
            allow_turn_on: true,
            phase: Phase::Idle,
            last_alarm: None,
        };
        actor.log(format!(
            "актор створено: щаблі {:?} квар, найменший {:.1} квар, уставка {:.1} квар, пристрій {}:{} (unit {}, регістр {})",
            actor.state.banks_kvar,
            actor.params.min_step_kvar,
            actor.params.target_kvar,
            actor.target.ip,
            actor.target.port,
            actor.target.unit,
            actor.target.register
        ));
        actor
    }

    fn make_params(state: &CondensatorState) -> Params {
        let s = state.min_step();
        Params {
            target_kvar: state.setpoint(),
            min_step_kvar: s,
            w_over: 2.0,
            epsilon_kvar: 0.3 * s,
            max_changes: 3,
            v_max: 420.0,
            v_reserve: 5.0,
            kappa_v_per_kvar: 0.0,
        }
    }

    // ───────────── Публічний інтерфейс ─────────────

    /// Напруга з боку 10 кВ. Оновлює ефективну ємність щаблів: c = c_nom * (V_банок / V_ном)².
    pub fn set_voltage(&mut self, _now: Instant, v10kv: f64) {
        // TODO: коридор валідності [0.8; 1.2] * V_ном і медіана за 3-5 відліків
        let v = v10kv as f32;
        if !v.is_finite() || v <= 0.0 {
            self.alarm(format!("некоректна напруга 10 кВ: {v10kv}, ігнорую"));
            return;
        }
        if self.last_v10.is_none() {
            self.log(format!(
                "перша напруга: {:.0} В на стороні 10 кВ = {:.1} В на шинах батарей (k = {})",
                v,
                self.state.bank_voltage(v),
                self.state.transformer_ratio
            ));
        }
        self.last_v10 = Some(v);
        let ratio = self.state.bank_voltage(v) / self.state.bank_voltage_nom;
        let k = ratio * ratio;
        for i in 0..self.banks.len() {
            self.banks[i].c_eff = self.state.banks_kvar[i] * k;
        }
    }

    /// Фактична маска реле (опитування). Джерело істини про стан.
    /// Якщо йде план і наш крок підтверджено, повертає наступний крок.
    pub fn set_mask(&mut self, now: Instant, mask: u16) -> Option<ModbusCommand> {
        self.on_feedback(mask, now);

        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Sending { steps, idx, prev, id, acked, bad_reads, deadline, shed } = phase {
            let sent = steps[idx];
            let changed = sent ^ prev; // біти, які цей крок міняв
            let matches = (mask ^ sent) & changed == 0; // чужі біти не рахуємо

            if matches {
                self.log(format!(
                    "крок {}/{} підтверджено: маска реле {}",
                    idx + 1,
                    steps.len(),
                    bits(mask)
                ));
                return self.next_step(steps, idx, now, shed);
            }
            if now >= deadline {
                self.alarm(format!(
                    "реле не виконало крок {}/{} вчасно (очікувалась {}, зчитано {}), план скасовано",
                    idx + 1,
                    steps.len(),
                    bits(sent),
                    bits(mask)
                ));
                self.enter_settling();
                return None;
            }
            // Розбіжність має вагу лише ПІСЛЯ підтвердження пристрою, і лише двічі поспіль
            // (одна маска могла бути прочитана до того, як запис спрацював).
            let mut bad_reads = bad_reads;
            if acked {
                bad_reads += 1;
                if bad_reads >= 2 {
                    self.alarm(format!(
                        "пристрій підтвердив запис, але реле не змінились (очікувалась {}, зчитано {}), план скасовано",
                        bits(sent),
                        bits(mask)
                    ));
                    self.enter_settling();
                    return None;
                }
            }
            self.log(format!(
                "крок {}/{}: маска {} ще не збіглась з очікуваною {} (розбіжних зчитувань після квитанції: {})",
                idx + 1,
                steps.len(),
                bits(mask),
                bits(sent),
                bad_reads
            ));
            self.phase = Phase::Sending { steps, idx, prev, id, acked, bad_reads, deadline, shed };
            return None;
        }
        self.phase = phase;
        None
    }

    /// Відлік Q. Якщо час приймати рішення, повертає перший крок нового плану.
    pub fn handle_q(&mut self, now: Instant, q: f64) -> Option<ModbusCommand> {
        let q = q as f32;
        if !q.is_finite() {
            self.alarm(format!("некоректне Q: {q}, ігнорую"));
            return None; // сміття не годує сторожа
        }
        self.last_q_at = now;

        if matches!(self.phase, Phase::Idle) {
            self.q_buf.push(q);
            return self.decide(now);
        }

        if let Phase::Failsafe { fresh, .. } = &mut self.phase {
            *fresh += 1;
            return None;
        }

        if matches!(self.phase, Phase::Handoff { .. }) {
            return self.handoff_q(now, q);
        }

        // Settling (у Sending відліки не потрібні: буфер все одно чиститься після плану)
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
                    self.log(format!("квитанція: пристрій прийняв запис (id {cid})"));
                } else {
                    self.log(format!("квитанція id {cid} не стосується поточного кроку, ігнорую"));
                }
            }
            CommandCallback::Unsatisfied(cid) => {
                if ours_failed {
                    self.alarm(format!("команду id {cid} не доставлено, план скасовано"));
                    self.enter_settling();
                } else {
                    self.alarm(format!("команду id {cid} не доставлено (не поточний крок)"));
                }
            }
        }
    }

    /// Таймаути й Failsafe. Можна викликати по тіку або в момент `next_deadline()`.
    pub fn on_tick(&mut self, now: Instant) -> Option<ModbusCommand> {
        // 1. Уже в Failsafe: гасимо наступний щабель, коли настав час
        if matches!(self.phase, Phase::Failsafe { .. }) {
            return self.failsafe_tick(now);
        }

        // 2. Q давно не оновлювався -> Failsafe (поріг залежить від КГУ)
        let silent = now.saturating_duration_since(self.last_q_at);
        if silent >= self.stale_limit() {
            self.alarm(format!(
                "немає Q вже {:.0} с ({}), вмикаю Failsafe",
                silent.as_secs_f32(),
                if self.kgu_on { "КГУ ввімкнений: острів" } else { "аварія зв'язку" }
            ));
            self.enter_failsafe(now);
            return self.failsafe_tick(now);
        }

        // 3a. М'яке вимкнення: Q так і не стабілізувався -> зупиняємось, решту щаблів лишаємо
        let handoff_left = match &self.phase {
            Phase::Handoff { deadline, steps, next, .. } => {
                if now >= *deadline {
                    Some(steps.len() - *next)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(left) = handoff_left {
            self.alarm(format!(
                "м'яке вимкнення: Q не стабілізувався за {:.0} с, зупиняюсь, лишилось вимкнути {} щабл(ів)",
                self.cfg.shed_timeout.as_secs_f32(),
                left
            ));
            self.enter_settling();
            return None;
        }

        // 3b. Крок плану не виконався вчасно -> скасовуємо план
        let timed_out = match &self.phase {
            Phase::Sending { deadline, .. } => now >= *deadline,
            _ => false,
        };
        if timed_out {
            self.alarm("час очікування кроку плану вичерпано, план скасовано");
            self.enter_settling();
        }
        None
    }

    /// Найближчий момент, коли актору потрібно прокинутись без нових даних.
    pub fn next_deadline(&self) -> Option<Instant> {
        match &self.phase {
            Phase::Failsafe { to_off, next_at, .. } => {
                if to_off.is_empty() {
                    None // чекаємо свіжих даних, а не годинника
                } else {
                    Some(*next_at)
                }
            }
            other => {
                let mut best = self.last_q_at + self.stale_limit();
                if let Phase::Sending { deadline, .. } | Phase::Handoff { deadline, .. } = other {
                    if *deadline < best {
                        best = *deadline;
                    }
                }
                Some(best)
            }
        }
    }

    /// Stop current work and discard Q; retain actual relay states and embargoes.
    pub fn pause_regulation(&mut self, now: Instant) {
        self.phase = Phase::Idle;
        self.q_buf.clear();
        self.last_q_at = now;
    }

    pub fn set_kgu(&mut self, on: bool) {
        if self.kgu_on != on {
            self.log(format!("КГУ {}", if on { "ввімкнено" } else { "вимкнено" }));
        }
        self.kgu_on = on;
    }

    pub fn set_allow_turn_on(&mut self, allow: bool) {
        if self.allow_turn_on != allow {
            self.log(format!("дозвіл вмикати щаблі: {}", if allow { "так" } else { "ні" }));
        }
        self.allow_turn_on = allow;
    }

    /// М'який перехід між режимами: вимикає всі увімкнені щаблі по одному, від найменшого.
    /// Перший крок повертається одразу, решту актор випускає сам (по set_mask і handle_q),
    /// коли Q стабілізується, тобто коли КГУ підхопив зміну. Нову стратегію актор бере замість
    /// поточного плану. Сам вмикання не забороняє: викликай разом із set_allow_turn_on(false).
    pub fn start_soft_shutdown(&mut self, now: Instant) -> Option<ModbusCommand> {
        if !self.have_relays {
            self.alarm("м'яке вимкнення неможливе: стан реле ще невідомий");
            return None;
        }
        if matches!(self.phase, Phase::Failsafe { .. }) {
            self.log("Failsafe вже гасить щаблі, м'яке вимкнення не потрібне".to_string());
            return None;
        }
        let steps = shutdown_steps(&self.banks);
        if steps.is_empty() {
            self.log("м'яке вимкнення: усі щаблі вже вимкнені".to_string());
            return None;
        }
        if matches!(self.phase, Phase::Sending { .. } | Phase::Handoff { .. }) {
            self.log("м'яке вимкнення замінює поточний план".to_string());
        }
        self.q_buf.clear();
        let prev = current_mask(&self.banks);
        self.log(format!(
            "м'яке вимкнення: {} щабл(ів), {} -> {} (від найменшого, м'яке ембарго min_on не враховується)",
            steps.len(),
            bits(prev),
            steps_str(&steps)
        ));
        Some(self.send_step(steps, 0, prev, now, true))
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Idle => "Idle",
            Phase::Sending { .. } => "Sending",
            Phase::Handoff { .. } => "Handoff",
            Phase::Settling { .. } => "Settling",
            Phase::Failsafe { .. } => "Failsafe",
        }
    }

    pub fn banks(&self) -> &[Bank] {
        &self.banks
    }

    /// Остання тривога (для UI й тестів).
    pub fn last_alarm(&self) -> Option<&str> {
        self.last_alarm.as_deref()
    }

    // ───────────── Внутрішнє: логи, стан і ембарго ─────────────

    fn log(&self, msg: String) {
        printers::event(format!("Конденсатори: {msg}"));
    }

    fn alarm(&mut self, msg: impl Into<String>) {
        let msg: String = msg.into();
        printers::err(format!("Конденсатори: {msg}"));
        self.last_alarm = Some(msg);
    }

    fn stale_limit(&self) -> Duration {
        if self.kgu_on {
            self.cfg.q_stale_island
        } else {
            self.cfg.q_stale_grid
        }
    }

    /// Єдине місце, де ставиться ембарго: на кожній зміні біта у фактичній масці.
    fn on_feedback(&mut self, mask: u16, now: Instant) {
        if !self.have_relays {
            // перша маска після старту: просто запам'ятовуємо, ембарго не чіпаємо
            for i in 0..self.banks.len() {
                self.banks[i].on = (mask >> i) & 1 == 1;
            }
            self.have_relays = true;
            self.log(format!("перша маска реле після старту: {} (ембарго не чіпаю)", bits(mask)));
            return;
        }

        let old_mask = current_mask(&self.banks);
        for i in 0..self.banks.len() {
            let is_on = (mask >> i) & 1 == 1;
            if self.banks[i].on == is_on {
                continue;
            }
            self.banks[i].on = is_on;
            if is_on {
                // щойно ввімкнули: м'яке ембарго на вимкнення (пускач)
                let hold = self.state.effective_min_on();
                self.banks[i].no_off_before = now + hold;
                self.log(format!(
                    "щабель {} ({:.0} квар) ввімкнено, не вимикати {:.0} с",
                    i,
                    self.banks[i].c_eff,
                    hold.as_secs_f32()
                ));
            } else {
                // щойно вимкнули: жорстке ембарго на вмикання (розряд)
                let hold = self.state.effective_min_off(i);
                self.banks[i].no_on_before = now + hold;
                self.log(format!(
                    "щабель {} ({:.0} квар) вимкнено, не вмикати {:.0} с",
                    i,
                    self.banks[i].c_eff,
                    hold.as_secs_f32()
                ));
            }
        }
        if old_mask != mask {
            self.log(format!("маска реле: {} -> {}", bits(old_mask), bits(mask)));
        }
    }

    fn make_command(&self, mask: u16) -> ModbusCommand {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed);
        ModbusCommand {
            id,
            state_register: self.target.register,
            new_state: vec![mask],
            target_address: self.target.unit,
            target_ip: Arc::clone(&self.target.ip),
            target_port: self.target.port,
            write_command: self.target.write,
        }
    }

    // ───────────── Рішення і виконання плану ─────────────

    fn v_bank(&self) -> Option<f32> {
        match self.last_v10 {
            Some(v) => Some(self.state.bank_voltage(v)),
            None => None,
        }
    }

    fn can_turn_on(&self) -> bool {
        self.allow_turn_on && self.v_bank().is_some()
    }

    /// Який Q віддати планувальнику.
    fn pick_q(&self) -> Option<f32> {
        let s = self.params.min_step_kvar;
        // Швидкий шлях: два останні відліки поспіль у зоні перекомпенсації
        let a = self.q_buf.get_back(0);
        let b = self.q_buf.get_back(1);
        if a.is_some() && b.is_some() {
            let a = a.unwrap();
            let b = b.unwrap();
            if a < -0.5 * s && b < -0.5 * s {
                return Some((a + b) / 2.0);
            }
        }
        // Повільний шлях: повне вікно
        self.q_buf.mean_last(SLOW_WINDOW)
    }

    fn decide(&mut self, now: Instant) -> Option<ModbusCommand> {
        if !self.have_relays {
            return None; // не знаємо фактичного стану реле
        }
        let q = match self.pick_q() {
            Some(q) => q,
            None => return None,
        };

        let input = Input {
            q_kvar: q,
            v_bank: self.v_bank().unwrap_or(0.0),
            now,
            allow_turn_on: self.can_turn_on(),
        };

        let p = match plan(&self.banks, &input, &self.params) {
            Some(p) => p,
            None => return None,
        };

        self.log(format!(
            "рішення: Q = {:.1} квар (ціль {:.1}), план з {} кроків: {} -> {}, очікувана зміна Q {:+.1} квар",
            q,
            self.params.target_kvar,
            p.steps.len(),
            bits(current_mask(&self.banks)),
            steps_str(&p.steps),
            p.expected_dq_kvar
        ));

        let prev = current_mask(&self.banks);
        Some(self.send_step(p.steps, 0, prev, now, false))
    }

    /// Крок підтверджено: відправляємо наступний або завершуємо план.
    /// Для м'якого вимкнення (shed) наступний крок іде не одразу: спершу чекаємо стабілізації Q.
    fn next_step(&mut self, steps: Vec<u16>, idx: usize, now: Instant, shed: bool) -> Option<ModbusCommand> {
        let next = idx + 1;
        if next >= steps.len() {
            self.log(format!("план виконано ({} кроків)", steps.len()));
            self.enter_settling();
            return None;
        }
        let prev = steps[idx];

        if shed {
            self.q_buf.clear();
            self.log(format!(
                "м'яке вимкнення: крок {}/{} виконано, чекаю стабілізації Q перед наступним",
                idx + 1,
                steps.len()
            ));
            self.phase = Phase::Handoff {
                steps,
                next,
                prev,
                skip: self.cfg.settle_skip,
                deadline: now + self.cfg.shed_timeout,
            };
            return None;
        }

        // Режим міг змінитися за час виконання: не вмикаємо, якщо вже заборонено
        let turns_on = steps[next] & !prev != 0;
        if turns_on && !self.can_turn_on() {
            self.alarm("вмикання заборонено посеред плану (режим змінився), план скасовано");
            self.enter_settling();
            return None;
        }

        Some(self.send_step(steps, next, prev, now, false))
    }

    /// Записуємо крок `idx` плану й переходимо в очікування його підтвердження.
    fn send_step(&mut self, steps: Vec<u16>, idx: usize, prev: u16, now: Instant, shed: bool) -> ModbusCommand {
        let mask = steps[idx];
        let cmd = self.make_command(mask);
        self.log(format!(
            "крок {}/{}: записую маску {} (id {}){}",
            idx + 1,
            steps.len(),
            bits(mask),
            cmd.id,
            if shed { " [м'яке вимкнення]" } else { "" }
        ));
        self.phase = Phase::Sending {
            steps,
            idx,
            prev,
            id: cmd.id,
            acked: false,
            bad_reads: 0,
            deadline: now + self.cfg.step_timeout,
            shed,
        };
        cmd
    }

    /// Відлік Q під час очікування в м'якому вимкненні: відкидаємо перші, збираємо, і коли
    /// середнє не вище `ціль + допуск`, випускаємо наступний крок.
    fn handoff_q(&mut self, now: Instant, q: f32) -> Option<ModbusCommand> {
        if let Phase::Handoff { skip, .. } = &mut self.phase {
            if *skip > 0 {
                *skip -= 1;
                return None;
            }
        }
        self.q_buf.push(q);
        let need = (self.cfg.settle_samples as usize).max(1);
        let mean = self.q_buf.mean_last(need)?; // відліків ще замало
        let limit = self.params.target_kvar + self.cfg.shed_q_tolerance_steps * self.params.min_step_kvar;
        if mean > limit {
            self.log(format!(
                "м'яке вимкнення: Q = {:.1} квар вище {:.1}, чекаю, поки КГУ підхопить",
                mean, limit
            ));
            return None;
        }

        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Handoff { steps, next, prev, .. } = phase {
            self.log(format!(
                "м'яке вимкнення: Q = {:.1} квар у нормі (<= {:.1}), наступний крок",
                mean, limit
            ));
            return Some(self.send_step(steps, next, prev, now, true));
        }
        self.phase = phase;
        None
    }

    fn enter_settling(&mut self) {
        self.q_buf.clear(); // у буфері відліки з проміжних станів, для рішення вони безглузді
        self.phase = Phase::Settling { skip: self.cfg.settle_skip, need: self.cfg.settle_samples };
        self.log(format!(
            "стабілізація: відкидаю {} відлік(ів) Q, збираю {}",
            self.cfg.settle_skip, self.cfg.settle_samples
        ));
    }

    // ───────────── Failsafe ─────────────

    fn enter_failsafe(&mut self, now: Instant) {
        self.q_buf.clear();
        let to_off = self.on_banks_largest_first();
        self.log(format!("Failsafe: гасимо щаблі {:?} (найбільші першими), пауза {:.0} с", to_off, self.cfg.failsafe_step.as_secs_f32()));
        self.phase = Phase::Failsafe {
            mask: current_mask(&self.banks),
            to_off,
            fresh: 0,
            next_at: now,
        };
    }

    /// Увімкнені щаблі, найбільші першими (швидше знімають перекомпенсацію).
    fn on_banks_largest_first(&self) -> Vec<usize> {
        let mut left: Vec<usize> = Vec::new();
        for i in 0..self.banks.len() {
            if self.banks[i].on {
                left.push(i);
            }
        }
        let mut ordered: Vec<usize> = Vec::new();
        while !left.is_empty() {
            let mut best_pos = 0;
            for pos in 1..left.len() {
                if self.banks[left[pos]].c_eff > self.banks[left[best_pos]].c_eff {
                    best_pos = pos;
                }
            }
            ordered.push(left.remove(best_pos));
        }
        ordered
    }

    /// Гасимо наступний щабель сліпо, без очікування підтвердження.
    fn failsafe_tick(&mut self, now: Instant) -> Option<ModbusCommand> {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let mut out = None;
        if let Phase::Failsafe { mut mask, mut to_off, fresh, mut next_at } = phase {
            if !to_off.is_empty() {
                if now >= next_at {
                    let i = to_off.remove(0);
                    mask &= !(1 << i);
                    // Тут ембарго ставимо в момент відправки, а не підтвердження:
                    // зворотного зв'язку може не бути взагалі. Це безпечний бік помилки.
                    self.banks[i].no_on_before = now + self.state.effective_min_off(i);
                    next_at = now + self.cfg.failsafe_step;
                    let cmd = self.make_command(mask);
                    self.log(format!(
                        "Failsafe: гашу щабель {} ({:.0} квар), маска {} (id {}), лишилось {}",
                        i,
                        self.banks[i].c_eff,
                        bits(mask),
                        cmd.id,
                        to_off.len()
                    ));
                    out = Some(cmd);
                }
                self.phase = Phase::Failsafe { mask, to_off, fresh, next_at };
            } else if fresh >= self.cfg.failsafe_exit_samples {
                self.log(format!("Failsafe: дані повернулись ({fresh} свіжих відліків), виходжу"));
                self.enter_settling();
            } else {
                self.phase = Phase::Failsafe { mask, to_off, fresh, next_at };
            }
        } else {
            self.phase = phase;
        }
        out
    }
}