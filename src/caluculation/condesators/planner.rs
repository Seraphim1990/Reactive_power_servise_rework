
//! Планувальник щаблів. Чиста логіка, без IO.
//! Конвенція: Q > 0 = завод споживає реактив із мережі (недокомпенсація).
//! Увімкнення щабля ЗМЕНШУЄ Q, вимкнення ЗБІЛЬШУЄ Q.

use std::time::Instant;

pub const MAX_BANKS: usize = 16;

// ───────────────────────── Типи ─────────────────────────

/// Один щабель. Номер у масиві = номер біта реле.
#[derive(Debug, Clone)]
pub struct Bank {
    /// Поточна оцінка віддачі, квар. 0 = щабля нема.
    pub c_eff: f32,
    /// Фактичний стан зі зворотного зв'язку (НЕ бажаний).
    pub on: bool,
    /// Жорстке ембарго: не вмикати раніше цього моменту (розряд конденсатора, min_off).
    pub no_on_before: Instant,
    /// М'яке ембарго: не вимикати раніше цього моменту (ресурс пускача, min_on).
    pub no_off_before: Instant,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub target_kvar: f32,      // уставка, напр. +1.5 * min_step
    pub min_step_kvar: f32,    // найменший щабель
    pub w_over: f32,           // штраф за перекомпенсацію (Q < 0), напр. 2.0
    pub epsilon_kvar: f32,     // мінімальний виграш, щоб діяти, напр. 0.3 * min_step
    pub max_changes: u32,      // стеля довжини черги: скільки щаблів змінити за одне рішення
    pub v_max: f32,            // напруга на шинах батарей, В (420)
    pub v_reserve: f32,        // запас до v_max, В
    pub kappa_v_per_kvar: f32, // підйом напруги на 1 квар; 0.0 = ще не оцінено
}

/// Вхід одного рішення.
#[derive(Debug, Clone, Copy)]
pub struct Input {
    /// Q, за яким приймаємо рішення (яке усереднення - вирішує той, хто викликає).
    pub q_kvar: f32,
    /// Напруга на шинах батарей, В (вже поділена на k).
    pub v_bank: f32,
    pub now: Instant,
    /// false = вмикати заборонено (вимикати можна завжди).
    pub allow_turn_on: bool,
}

/// Результат: послідовність масок реле. Кожен крок = не більше одного off і одного on.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub steps: Vec<u16>,
    pub expected_dq_kvar: f32, // на скільки очікуємо змінити Q (для аналітики)
}

// ───────────────────────── Допоміжні функції ─────────────────────────

/// Маска реле за фактичним станом щаблів.
pub fn current_mask(banks: &[Bank]) -> u16 {
    let mut mask: u16 = 0;
    for i in 0..banks.len() {
        if banks[i].on {
            mask |= 1 << i;
        }
    }
    mask
}

/// Вартість залишку r = Q після зміни. Менше = краще. Перекомпенсація (r < 0) дорожча.
fn cost(r: f32, p: &Params) -> f32 {
    let d = (r - p.target_kvar).abs();
    if r < 0.0 {
        d * p.w_over
    } else {
        d
    }
}

/// Чи можна цим щаблем зараз діяти (власне ембарго, а не глобальна пауза).
fn is_free(bank: &Bank, input: &Input, emergency_off: bool) -> bool {
    if bank.c_eff <= 0.0 {
        return false;
    }
    if bank.on {
        // вимкнення: безпечне; м'яке ембарго знімається при явній перекомпенсації
        input.now >= bank.no_off_before || emergency_off
    } else {
        // вмикання: жорстке ембарго ніколи не обходиться
        input.allow_turn_on && input.now >= bank.no_on_before
    }
}

/// Чи безпечна за напругою ця комбінація (перевіряється лише якщо щось вмикаємо).
fn voltage_ok(dq_turned_on_kvar: f32, input: &Input, p: &Params) -> bool {
    if dq_turned_on_kvar <= 0.0 {
        return true;
    }
    let v_predicted = input.v_bank + p.kappa_v_per_kvar * dq_turned_on_kvar;
    v_predicted <= p.v_max - p.v_reserve
}

/// Розбиває набір змін на кроки: у кожному кроці один off і один on (якщо є).
/// Порядок: з кінця списків, тобто щаблі з більшим індексом першими.
pub fn build_steps(banks: &[Bank], toggle: u16) -> Vec<u16> {
    let mut to_off: Vec<usize> = Vec::new();
    let mut to_on: Vec<usize> = Vec::new();
    for i in 0..banks.len() {
        if (toggle >> i) & 1 == 1 {
            if banks[i].on {
                to_off.push(i);
            } else {
                to_on.push(i);
            }
        }
    }

    let mut mask = current_mask(banks);
    let mut steps: Vec<u16> = Vec::new();
    while !to_off.is_empty() || !to_on.is_empty() {
        if let Some(i) = to_off.pop() {
            mask &= !(1 << i);
        }
        if let Some(i) = to_on.pop() {
            mask |= 1 << i;
        }
        steps.push(mask);
    }
    steps
}

/// Стратегія поступового вимкнення ВСІХ увімкнених щаблів: по одному за крок,
/// від найменшого за ємністю до найбільшого (при рівних - з меншим індексом).
/// Чому від найменшого: перші кроки найм'якші для того, хто підхоплює Q (КГУ), а якщо
/// перехід урветься посередині, у роботі лишаються найбільші щаблі, тобто основна компенсація.
/// Кожен крок - повна маска. Порожній результат = нічого вимикати.
/// М'яке ембарго min_on тут свідомо не враховується (перехід режимів важливіший за ресурс пускача).
pub fn shutdown_steps(banks: &[Bank]) -> Vec<u16> {
    // 1. Індекси увімкнених щаблів
    let mut left: Vec<usize> = Vec::new();
    for i in 0..banks.len() {
        if banks[i].on {
            left.push(i);
        }
    }

    // 2. Щоразу беремо найменший із тих, що лишились
    let mut mask = current_mask(banks);
    let mut steps: Vec<u16> = Vec::new();
    while !left.is_empty() {
        let mut best_pos = 0;
        for pos in 1..left.len() {
            if banks[left[pos]].c_eff < banks[left[best_pos]].c_eff {
                best_pos = pos;
            }
        }
        let i = left.remove(best_pos);
        mask &= !(1 << i);
        steps.push(mask);
    }
    steps
}

// ───────────────────────── Головна функція ─────────────────────────

/// Повертає None, якщо краще нічого не міняти.
pub fn plan(banks: &[Bank], input: &Input, p: &Params) -> Option<Plan> {
    assert!(banks.len() <= MAX_BANKS);

    let emergency_off = input.q_kvar < -2.0 * p.min_step_kvar;

    // 1. Вільні щаблі
    let mut free: Vec<usize> = Vec::new();
    for i in 0..banks.len() {
        if is_free(&banks[i], input, emergency_off) {
            free.push(i);
        }
    }
    if free.is_empty() {
        return None;
    }

    // 2. Початковий кандидат: «нічого не робити»
    let cost_now = cost(input.q_kvar, p);
    let mut best_cost = cost_now;
    let mut best_toggle: u16 = 0;
    let mut best_dq: f32 = 0.0;

    // 3. Перебір усіх непорожніх підмножин вільних щаблів.
    //    combo - число, де біт j каже: чіпаємо щабель free[j] чи ні.
    let n = free.len();
    for combo in 1u32..(1u32 << n) {
        if combo.count_ones() > p.max_changes {
            continue; // стеля довжини черги
        }

        let mut toggle: u16 = 0;
        let mut dq: f32 = 0.0;    // сумарна зміна Q
        let mut dq_on: f32 = 0.0; // скільки квар додаємо вмиканням
        for j in 0..n {
            if (combo >> j) & 1 == 0 {
                continue;
            }
            let i = free[j];
            toggle |= 1 << i;
            if banks[i].on {
                dq += banks[i].c_eff; // вимкнули -> Q зросло
            } else {
                dq -= banks[i].c_eff; // ввімкнули -> Q впало
                dq_on += banks[i].c_eff;
            }
        }

        if !voltage_ok(dq_on, input, p) {
            continue;
        }

        let c = cost(input.q_kvar + dq, p);
        if c < best_cost {
            best_cost = c;
            best_toggle = toggle;
            best_dq = dq;
        }
    }

    // 4. Діємо лише якщо виграш реальний
    if best_toggle == 0 || cost_now - best_cost <= p.epsilon_kvar {
        return None;
    }
    Some(Plan {
        steps: build_steps(banks, best_toggle),
        expected_dq_kvar: best_dq,
    })
}

// ───────────────────────── Тести ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn params() -> Params {
        Params {
            target_kvar: 24.0, min_step_kvar: 16.0, w_over: 2.0, epsilon_kvar: 5.0,
            max_changes: 3, v_max: 420.0, v_reserve: 5.0, kappa_v_per_kvar: 0.0,
        }
    }

    fn bank(c: f32, on: bool, now: Instant) -> Bank {
        Bank { c_eff: c, on, no_on_before: now, no_off_before: now }
    }

    fn input(q: f32, now: Instant) -> Input {
        Input { q_kvar: q, v_bank: 400.0, now, allow_turn_on: true }
    }

    #[test]
    fn undercompensation_turns_on_best_bank() {
        let now = Instant::now();
        let banks = vec![bank(16.0, false, now), bank(32.0, false, now), bank(64.0, false, now)];
        let plan = plan(&banks, &input(90.0, now), &params()).unwrap();
        assert_eq!(plan.steps, vec![0b100]);
    }

    #[test]
    fn overcompensation_turns_off() {
        let now = Instant::now();
        let banks = vec![bank(16.0, true, now), bank(32.0, true, now)];
        let plan = plan(&banks, &input(-40.0, now), &params()).unwrap();
        assert_eq!(*plan.steps.last().unwrap(), 0);
    }

    #[test]
    fn dead_zone_does_nothing() {
        let now = Instant::now();
        let banks = vec![bank(16.0, false, now), bank(32.0, false, now)];
        assert!(plan(&banks, &input(24.0, now), &params()).is_none());
    }

    #[test]
    fn hard_embargo_blocks_turn_on() {
        let now = Instant::now();
        let mut b = bank(64.0, false, now);
        b.no_on_before = now + Duration::from_secs(30);
        assert!(plan(&[b], &input(90.0, now), &params()).is_none());
    }

    #[test]
    fn soft_embargo_lifted_only_on_overcompensation() {
        let now = Instant::now();
        let mut b = bank(32.0, true, now);
        b.no_off_before = now + Duration::from_secs(30);
        assert!(plan(&[b.clone()], &input(0.0, now), &params()).is_none());
        assert!(plan(&[b], &input(-40.0, now), &params()).is_some());
    }

    #[test]
    fn voltage_blocks_turn_on() {
        let now = Instant::now();
        let banks = vec![bank(64.0, false, now)];
        let mut inp = input(90.0, now);
        inp.v_bank = 416.0; // > 420 - 5
        assert!(plan(&banks, &inp, &params()).is_none());
    }

    #[test]
    fn allow_turn_on_false_blocks() {
        let now = Instant::now();
        let banks = vec![bank(64.0, false, now)];
        let mut inp = input(90.0, now);
        inp.allow_turn_on = false;
        assert!(plan(&banks, &inp, &params()).is_none());
    }

    #[test]
    fn shutdown_goes_smallest_first_one_bank_per_step() {
        let now = Instant::now();
        let banks = vec![bank(32.0, true, now), bank(16.0, true, now), bank(64.0, true, now)];
        // найменший (біт 1), потім біт 0, потім біт 2
        assert_eq!(shutdown_steps(&banks), vec![0b101, 0b100, 0b000]);
    }

    #[test]
    fn shutdown_touches_only_banks_that_are_on() {
        let now = Instant::now();
        let banks = vec![bank(16.0, true, now), bank(32.0, false, now), bank(64.0, true, now)];
        assert_eq!(shutdown_steps(&banks), vec![0b100, 0b000]);
        let all_off = vec![bank(16.0, false, now), bank(32.0, false, now)];
        assert!(shutdown_steps(&all_off).is_empty());
    }

    #[test]
    fn shutdown_ties_go_by_index_and_last_step_is_zero() {
        let now = Instant::now();
        let banks = vec![bank(16.0, true, now), bank(16.0, true, now), bank(16.0, true, now)];
        let steps = shutdown_steps(&banks);
        assert_eq!(steps, vec![0b110, 0b100, 0b000]);
        assert_eq!(*steps.last().unwrap(), 0);
    }

    #[test]
    fn steps_one_off_and_one_on_per_step() {
        let now = Instant::now();
        let banks = vec![bank(16.0, true, now), bank(16.0, false, now),
                         bank(16.0, false, now), bank(16.0, false, now)];
        let steps = build_steps(&banks, 0b1111);
        // off(0)+on(3), потім on(2), потім on(1)
        assert_eq!(steps, vec![0b1000, 0b1100, 0b1110]);
    }
}
 