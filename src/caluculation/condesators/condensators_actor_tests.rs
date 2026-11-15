//! Тести актора. Використовують лише публічний інтерфейс, тож лежать окремим файлом.
//! Підключення в mod.rs:  #[cfg(test)] mod condensators_actor_tests;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::condensators_actor::{ActorConfig, CondensatorActor, Target};
use super::condensators_state::CondensatorState;
use crate::meta::state::command::{CommandCallback, ModbusCommand, WriteCommand};

fn t(base: Instant, secs: u64) -> Instant {
    base + Duration::from_secs(secs)
}

fn test_state() -> CondensatorState {
    CondensatorState {
        banks_kvar: vec![16.0, 32.0, 64.0],
        transformer_ratio: 26.5,
        min_off_secs: vec![60.0, 60.0, 60.0],
        min_on_secs: 30.0,
        bank_voltage_nom: 400.0,
        safety_margin: 0.05,
        poll_period_secs: 2.0,
    }
}

fn test_target() -> Target {
    Target {
        ip: Arc::new(String::from("10.0.0.5")),
        port: 502,
        unit: 1,
        register: 470,
        write: WriteCommand::Multi,
    }
}

fn make_with(base: Instant, counter: Arc<AtomicU64>) -> CondensatorActor {
    CondensatorActor::new(test_state(), ActorConfig::default(), test_target(), counter, base)
}

fn make(base: Instant) -> CondensatorActor {
    make_with(base, Arc::new(AtomicU64::new(1)))
}

/// Маска, яку несе команда.
fn mask_of(cmd: &Option<ModbusCommand>) -> Option<u16> {
    cmd.as_ref().map(|c| c.new_state[0])
}

/// Подає `count` відліків Q з кроком 1 с, збирає всі повернені команди.
fn feed_q(a: &mut CondensatorActor, base: Instant, from: u64, count: u64, q: f64) -> Vec<ModbusCommand> {
    let mut all = Vec::new();
    for k in 0..count {
        if let Some(c) = a.handle_q(t(base, from + k), q) {
            all.push(c);
        }
    }
    all
}

/// Актор з відомими реле (усі вимкнені) і напругою 400 В на шинах батарей.
fn ready(base: Instant) -> CondensatorActor {
    let mut a = make(base);
    a.set_mask(t(base, 1), 0);
    a.set_voltage(t(base, 1), 10600.0); // 10600 / 26.5 = 400 В
    a
}

fn happy(a: &mut CondensatorActor, now: Instant, c: &ModbusCommand) {
    a.income_callback(now, CommandCallback::Happy(c.id));
}

#[test]
fn no_decisions_before_relay_state_is_known() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_voltage(t(base, 1), 10600.0);
    assert!(feed_q(&mut a, base, 100, 12, 90.0).is_empty());
}

#[test]
fn first_step_carries_target_and_register() {
    let base = Instant::now();
    let mut a = ready(base);
    assert!(feed_q(&mut a, base, 100, 9, 90.0).is_empty()); // вікно ще не повне
    let c = a.handle_q(t(base, 109), 90.0).unwrap();
    assert_eq!(c.new_state, vec![0b100]); // вмикаємо найбільший щабель
    assert_eq!(c.state_register, 470);
    assert_eq!(c.target_address, 1);
    assert_eq!(c.target_port, 502);
    assert_eq!(*c.target_ip, "10.0.0.5");
    assert_eq!(c.write_command, WriteCommand::Multi);
    assert_eq!(a.phase_name(), "Sending");
}

#[test]
fn plan_advances_on_confirmed_mask_and_ids_increase() {
    let base = Instant::now();
    let mut a = ready(base);
    let c1 = feed_q(&mut a, base, 100, 10, 130.0).pop().unwrap();
    assert_eq!(c1.new_state, vec![0b100]);

    assert!(a.set_mask(t(base, 110), 0).is_none()); // реле ще старе
    happy(&mut a, t(base, 110), &c1);
    let c2 = a.set_mask(t(base, 112), 0b100).unwrap();
    assert_eq!(c2.new_state, vec![0b110]);
    happy(&mut a, t(base, 112), &c2);
    let c3 = a.set_mask(t(base, 114), 0b110).unwrap();
    assert_eq!(c3.new_state, vec![0b111]);
    assert!(c1.id < c2.id && c2.id < c3.id);

    happy(&mut a, t(base, 114), &c3);
    assert!(a.set_mask(t(base, 116), 0b111).is_none()); // план вичерпано
    assert_eq!(a.phase_name(), "Settling");

    a.handle_q(t(base, 117), 18.0); // відкинутий
    assert_eq!(a.phase_name(), "Settling");
    a.handle_q(t(base, 118), 18.0);
    a.handle_q(t(base, 119), 18.0);
    assert_eq!(a.phase_name(), "Idle");
}

#[test]
fn matching_mask_confirms_step_even_before_ack() {
    let base = Instant::now();
    let mut a = ready(base);
    feed_q(&mut a, base, 100, 10, 130.0);
    // маска прийшла раніше за квитанцію: джерело істини - маска
    let c2 = a.set_mask(t(base, 110), 0b100);
    assert_eq!(mask_of(&c2), Some(0b110));
}

#[test]
fn foreign_bit_change_does_not_block_confirmation() {
    let base = Instant::now();
    let mut a = ready(base);
    feed_q(&mut a, base, 100, 10, 130.0);
    let c2 = a.set_mask(t(base, 111), 0b101); // наш біт 2 + чужий біт 0
    assert_eq!(mask_of(&c2), Some(0b110));
}

#[test]
fn unsatisfied_delivery_cancels_plan() {
    let base = Instant::now();
    let mut a = ready(base);
    let c1 = feed_q(&mut a, base, 100, 10, 130.0).pop().unwrap();
    a.income_callback(t(base, 110), CommandCallback::Unsatisfied(c1.id));
    assert_eq!(a.phase_name(), "Settling");
    assert!(a.last_alarm().is_some());
}

#[test]
fn ack_with_foreign_id_is_ignored_and_mismatch_counts_only_after_own_ack() {
    let base = Instant::now();
    let mut a = ready(base);
    let c1 = feed_q(&mut a, base, 100, 10, 130.0).pop().unwrap();

    // чужа (стара) квитанція: не вважається підтвердженням
    a.income_callback(t(base, 110), CommandCallback::Happy(c1.id + 100));
    assert!(a.set_mask(t(base, 110), 0).is_none());
    assert!(a.set_mask(t(base, 112), 0).is_none());
    assert_eq!(a.phase_name(), "Sending");

    // своя квитанція: тепер дві розбіжності поспіль скасовують план
    happy(&mut a, t(base, 113), &c1);
    assert!(a.set_mask(t(base, 113), 0).is_none());
    assert_eq!(a.phase_name(), "Sending"); // одна розбіжність: ще не вирок
    assert!(a.set_mask(t(base, 114), 0).is_none());
    assert_eq!(a.phase_name(), "Settling");
    assert!(a.last_alarm().is_some());
}

#[test]
fn step_timeout_cancels_plan() {
    let base = Instant::now();
    let mut a = ready(base);
    feed_q(&mut a, base, 100, 10, 130.0); // запис о t=109, дедлайн t=117
    assert!(a.on_tick(t(base, 116)).is_none());
    assert_eq!(a.phase_name(), "Sending");
    assert!(a.on_tick(t(base, 118)).is_none());
    assert_eq!(a.phase_name(), "Settling");
    assert!(a.last_alarm().is_some());
}

#[test]
fn no_turn_on_without_voltage() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0); // реле знаємо, напруги нема
    assert!(feed_q(&mut a, base, 100, 12, 90.0).is_empty());
    a.set_voltage(t(base, 112), 10600.0);
    let c = a.handle_q(t(base, 113), 90.0);
    assert_eq!(mask_of(&c), Some(0b100));
}

#[test]
fn allow_turn_on_false_blocks_planning() {
    let base = Instant::now();
    let mut a = ready(base);
    a.set_allow_turn_on(false);
    assert!(feed_q(&mut a, base, 100, 12, 90.0).is_empty());
}

#[test]
fn fast_path_turns_off_after_two_overcompensated_samples() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b111);
    assert!(a.handle_q(t(base, 100), -40.0).is_none());
    let c = a.handle_q(t(base, 101), -40.0);
    assert_eq!(mask_of(&c), Some(0b011)); // вимкнули найбільший
}

#[test]
fn feedback_sets_embargo_only_on_observed_change() {
    let base = Instant::now();
    let mut a = ready(base);
    a.set_mask(t(base, 50), 0b001);
    let soft = a.banks()[0].no_off_before;
    assert!(soft > t(base, 50));
    a.set_mask(t(base, 60), 0b001);
    assert_eq!(a.banks()[0].no_off_before, soft); // не посунулось
    a.set_mask(t(base, 70), 0);
    // жорстке ембарго на вмикання: 60*1.05 + 2 = 65 с
    assert!(a.banks()[0].no_on_before >= t(base, 70) + Duration::from_secs(65));
}

#[test]
fn voltage_scales_bank_capacity_by_square() {
    let base = Instant::now();
    let mut a = make(base);
    assert!((a.banks()[2].c_eff - 64.0).abs() < 0.01);
    a.set_voltage(t(base, 1), 10600.0 * 1.05); // 420 В на шинах батарей: +5%
    assert!((a.banks()[2].c_eff - 64.0 * 1.1025).abs() < 0.05);
    // сміття не чіпає ємності
    a.set_voltage(t(base, 2), f64::NAN);
    a.set_voltage(t(base, 3), -5.0);
    assert!((a.banks()[2].c_eff - 64.0 * 1.1025).abs() < 0.05);
}

#[test]
fn island_failsafe_turns_banks_off_one_per_step() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b111);
    a.set_kgu(true);
    assert!(a.on_tick(t(base, 19)).is_none()); // 19 с тиші: ще рано (поріг 20)

    assert_eq!(mask_of(&a.on_tick(t(base, 21))), Some(0b011)); // найбільший (64) першим
    assert!(a.last_alarm().is_some());
    assert!(a.on_tick(t(base, 21)).is_none()); // пауза між щаблями ще не минула
    assert_eq!(mask_of(&a.on_tick(t(base, 22))), Some(0b001));
    assert_eq!(mask_of(&a.on_tick(t(base, 23))), Some(0b000));
    assert!(a.on_tick(t(base, 24)).is_none());
    assert_eq!(a.phase_name(), "Failsafe");
    assert!(a.banks()[2].no_on_before >= t(base, 21) + Duration::from_secs(65));
}

#[test]
fn grid_failsafe_waits_longer() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b111);
    assert!(a.on_tick(t(base, 30)).is_none()); // КГУ вимкнений: поріг 60 с
    assert!(a.on_tick(t(base, 61)).is_some());
}

#[test]
fn failsafe_exits_after_fresh_samples_and_finished_sequence() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b001);
    a.set_kgu(true);
    assert!(a.on_tick(t(base, 21)).is_some()); // єдиний щабель погашено
    for k in 0..4 {
        a.handle_q(t(base, 30 + k), 10.0);
    }
    a.on_tick(t(base, 35));
    assert_eq!(a.phase_name(), "Failsafe"); // свіжих відліків лише 4 з 5
    a.handle_q(t(base, 36), 10.0);
    a.on_tick(t(base, 37));
    assert_eq!(a.phase_name(), "Settling");
}

#[test]
fn garbage_q_does_not_feed_watchdog() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b111);
    a.set_kgu(true);
    a.handle_q(t(base, 15), f64::NAN);
    assert!(a.on_tick(t(base, 21)).is_some());
}

#[test]
fn next_deadline_follows_phase() {
    let base = Instant::now();
    let mut a = ready(base);
    // Idle: найближче - застій Q (КГУ вимкнений -> 60 с від останнього Q; тут last_q_at = base)
    assert_eq!(a.next_deadline(), Some(t(base, 60)));
    a.set_kgu(true);
    assert_eq!(a.next_deadline(), Some(t(base, 20)));

    // Sending: дедлайн кроку (t=109 + 8) раніше за застій (t=109 + 20)
    feed_q(&mut a, base, 100, 10, 130.0);
    assert_eq!(a.next_deadline(), Some(t(base, 117)));

    // Failsafe: наступне гасіння, а коли нічого гасити, годинник не потрібен
    let mut b = make(base);
    b.set_mask(t(base, 1), 0b011); // два щаблі ввімкнені
    b.set_kgu(true);
    b.on_tick(t(base, 21)); // погасили перший, лишився ще один
    assert_eq!(b.next_deadline(), Some(t(base, 22)));
    b.on_tick(t(base, 22)); // погасили другий: гасити більше нічого
    assert_eq!(b.next_deadline(), None);
}

#[test]
fn shared_counter_gives_unique_ids_across_actors() {
    let base = Instant::now();
    let counter = Arc::new(AtomicU64::new(1));
    let mut a = make_with(base, Arc::clone(&counter));
    let mut b = make_with(base, Arc::clone(&counter));
    a.set_mask(t(base, 1), 0b001);
    b.set_mask(t(base, 1), 0b001);
    a.set_kgu(true);
    b.set_kgu(true);
    let ca = a.on_tick(t(base, 21)).unwrap();
    let cb = b.on_tick(t(base, 21)).unwrap();
    assert_ne!(ca.id, cb.id);
}

// ───────────── М'яке вимкнення ─────────────

/// Актор, у якого всі три щаблі вже ввімкнені (маска 0b111).
fn all_on(base: Instant) -> CondensatorActor {
    let mut a = make(base);
    a.set_mask(t(base, 1), 0b111);
    a
}

#[test]
fn soft_shutdown_starts_with_smallest_bank() {
    let base = Instant::now();
    let mut a = all_on(base);
    let c = a.start_soft_shutdown(t(base, 10)).unwrap();
    assert_eq!(c.new_state, vec![0b110]); // щабель 0 (16 квар), найменший
    assert_eq!(a.phase_name(), "Sending");
}

#[test]
fn soft_shutdown_waits_for_q_to_settle_between_steps() {
    let base = Instant::now();
    let mut a = all_on(base);
    a.start_soft_shutdown(t(base, 10)).unwrap();

    // крок 1 підтверджено маскою: наступного кроку ще немає, чекаємо стабілізації Q
    assert!(a.set_mask(t(base, 11), 0b110).is_none());
    assert_eq!(a.phase_name(), "Handoff");

    // допуск: ціль 24 + 2 * 16 = 56 квар
    assert!(a.handle_q(t(base, 12), 80.0).is_none()); // перший відлік відкидається
    assert!(a.handle_q(t(base, 13), 80.0).is_none()); // відліків ще замало
    assert!(a.handle_q(t(base, 14), 80.0).is_none()); // середнє 80 > 56: КГУ ще не підхопив
    assert_eq!(a.phase_name(), "Handoff");

    // Q пішов униз: середнє двох останніх (80, 20) = 50 <= 56 -> наступний крок
    let c2 = a.handle_q(t(base, 15), 20.0).unwrap();
    assert_eq!(c2.new_state, vec![0b100]);
    assert_eq!(a.phase_name(), "Sending");
}

#[test]
fn soft_shutdown_runs_to_the_end_and_sets_embargo() {
    let base = Instant::now();
    let mut a = all_on(base);
    let c1 = a.start_soft_shutdown(t(base, 10)).unwrap();
    assert_eq!(c1.new_state, vec![0b110]);

    assert!(a.set_mask(t(base, 11), 0b110).is_none());
    a.handle_q(t(base, 12), 20.0); // відкинутий
    a.handle_q(t(base, 13), 20.0);
    let c2 = a.handle_q(t(base, 14), 20.0).unwrap(); // Q у нормі
    assert_eq!(c2.new_state, vec![0b100]);

    assert!(a.set_mask(t(base, 15), 0b100).is_none());
    a.handle_q(t(base, 16), 20.0);
    a.handle_q(t(base, 17), 20.0);
    let c3 = a.handle_q(t(base, 18), 20.0).unwrap();
    assert_eq!(c3.new_state, vec![0b000]);
    assert!(c1.id < c2.id && c2.id < c3.id);

    assert!(a.set_mask(t(base, 19), 0).is_none()); // останній крок
    assert_eq!(a.phase_name(), "Settling");
    // кожен щабель отримав жорстке ембарго від СВОГО моменту вимкнення (t = 11, 15, 19)
    let off_at = [11u64, 15, 19];
    for i in 0..3 {
        assert!(!a.banks()[i].on);
        assert!(a.banks()[i].no_on_before >= t(base, off_at[i]) + Duration::from_secs(65));
    }
    assert!(a.banks()[2].no_on_before > a.banks()[0].no_on_before); // пізніше вимкнений - довше тримаємо
}

#[test]
fn soft_shutdown_gives_up_when_q_never_settles() {
    let base = Instant::now();
    let mut a = all_on(base);
    a.start_soft_shutdown(t(base, 10)).unwrap();
    a.set_mask(t(base, 11), 0b110); // дедлайн очікування: t=11 + 60 = 71
    for k in 12..15 {
        a.handle_q(t(base, k), 80.0); // КГУ не підхоплює
    }
    assert_eq!(a.next_deadline(), Some(t(base, 71)));
    assert!(a.on_tick(t(base, 72)).is_none());
    assert_eq!(a.phase_name(), "Settling");
    assert!(a.last_alarm().is_some());
    // решту щаблів лишили увімкненими
    assert!(!a.banks()[0].on);
    assert!(a.banks()[1].on && a.banks()[2].on);
}

#[test]
fn soft_shutdown_overrides_min_on_embargo() {
    let base = Instant::now();
    let mut a = make(base);
    a.set_mask(t(base, 1), 0);
    a.set_mask(t(base, 50), 0b111); // щойно ввімкнулись: м'яке ембарго ~34 с
    let c = a.start_soft_shutdown(t(base, 51)).unwrap();
    assert_eq!(c.new_state, vec![0b110]);
}

#[test]
fn soft_shutdown_with_nothing_on_does_nothing() {
    let base = Instant::now();
    let mut a = ready(base); // усі вимкнені
    assert!(a.start_soft_shutdown(t(base, 10)).is_none());
    assert_eq!(a.phase_name(), "Idle");
}

#[test]
fn soft_shutdown_is_refused_without_relay_state_and_during_failsafe() {
    let base = Instant::now();
    let mut a = make(base); // маски реле ще не було
    assert!(a.start_soft_shutdown(t(base, 10)).is_none());
    assert!(a.last_alarm().is_some());

    let mut b = all_on(base);
    b.set_kgu(true);
    assert!(b.on_tick(t(base, 21)).is_some()); // Failsafe вже гасить
    assert!(b.start_soft_shutdown(t(base, 22)).is_none());
    assert_eq!(b.phase_name(), "Failsafe");
}

#[test]
fn soft_shutdown_replaces_running_plan_and_old_ack_does_not_count() {
    let base = Instant::now();
    let mut a = ready(base); // реле 0b000, напруга 400 В
    let c1 = feed_q(&mut a, base, 100, 10, 130.0).pop().unwrap(); // крок 1/3: 0b100
    let c2 = a.set_mask(t(base, 110), 0b100).unwrap(); // крок 2/3: 0b110, запис у польоті
    assert!(c1.id < c2.id);

    // нова стратегія замінює старий план: фактично ввімкнений лише щабель 2
    let shed = a.start_soft_shutdown(t(base, 111)).unwrap();
    assert_eq!(shed.new_state, vec![0b000]);
    assert!(shed.id > c2.id);
    assert_eq!(a.phase_name(), "Sending");

    // стара квитанція не стосується нового кроку, тож розбіжності без власної квитанції не рахуються
    a.income_callback(t(base, 111), CommandCallback::Happy(c2.id));
    a.set_mask(t(base, 112), 0b110);
    a.set_mask(t(base, 113), 0b110);
    assert_eq!(a.phase_name(), "Sending");
}