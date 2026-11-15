//! Шкала уставки cos для КГУ і підбір уставки за реактивом мережі. Чиста логіка, без IO.
//!
//! Уставка v = 0..=1000 (4-20 мА), s = 0.8 + 0.00025 * v:
//!   s <= 1  -> cos = s,     індуктивна сторона: КГУ ВИДАЄ реактив   (Q_gen > 0)
//!   s >  1  -> cos = 2 - s, ємнісна сторона:    КГУ СПОЖИВАЄ реактив (Q_gen < 0)
//! v = 800 це cos = 1 і РОЗРИВ: через нього Q міняє знак. Біля одиниці квант уставки
//! найгрубіший, тому навколо 800 тримаємо захисну зону (guard), де уставку не ставимо.
//!
//! Конвенція Q мережі: + = завод споживає з мережі (недокомпенсація).
//! Більше Q_gen -> менше Q мережі. Тому недокомпенсація => уставку ЗМЕНШУЄМО (до індуктивної).

const S_ZERO: f64 = 0.8;
const S_UNIT: f64 = 0.00025;

pub const V_MAX: i32 = 1000;
/// cos = 1.
pub const V_UNITY: i32 = 800;

fn clamp_v(v: i32) -> i32 {
    v.clamp(0, V_MAX)
}

/// Реактив КГУ (квар) при уставці `v` і активній потужності `p_kw`. + = видає.
pub fn q_of(v: i32, p_kw: f32) -> f32 {
    let s = S_ZERO + S_UNIT * clamp_v(v) as f64;
    let (cos, sign) = if s <= 1.0 { (s, 1.0) } else { (2.0 - s, -1.0) };
    let tan = (1.0 - cos * cos).max(0.0).sqrt() / cos;
    (sign * p_kw as f64 * tan) as f32
}

/// Найближча уставка для бажаного Q_gen. Потребує p_kw > 0.
fn v_of_q(q_kvar: f32, p_kw: f32) -> i32 {
    let phi = (q_kvar.abs() as f64).atan2(p_kw as f64);
    let cos = phi.cos();
    let s = if q_kvar >= 0.0 { cos } else { 2.0 - cos };
    clamp_v(((s - S_ZERO) / S_UNIT).round() as i32)
}

/// На скільки квар міняється Q_gen від ОДНІЄЇ одиниці уставки біля `v` (гірший із двох боків).
pub fn quantum(v: i32, p_kw: f32) -> f32 {
    let up = (q_of(v + 1, p_kw) - q_of(v, p_kw)).abs();
    let down = (q_of(v, p_kw) - q_of(v - 1, p_kw)).abs();
    up.max(down)
}

/// Уставка «парковки»: найближча до одиниці з індуктивного боку, поза захисною зоною.
pub fn park_value(guard: i32) -> i32 {
    V_UNITY - guard
}

/// Якщо v потрапило в захисну зону навколо 800, виштовхуємо на ту межу, що ближча за Q.
fn push_out_of_guard(v: i32, q_want: f32, p_kw: f32, guard: i32) -> i32 {
    if (v - V_UNITY).abs() >= guard {
        return v;
    }
    let low = V_UNITY - guard;
    let high = V_UNITY + guard;
    let d_low = (q_of(low, p_kw) - q_want).abs();
    let d_high = (q_of(high, p_kw) - q_want).abs();
    if d_low <= d_high {
        low
    } else {
        high
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TrimParams {
    /// Бажаний Q мережі, квар (+ = трохи недокомпенсації), напр. 12.5 для коридору 10..15.
    pub target_kvar: f32,
    /// Нижня межа гістерезису: шум вимірювання Q. Мало сенсу ставити менше за реальний шум.
    pub deadband_floor_kvar: f32,
    /// Півширина захисної зони навколо 800, в одиницях уставки.
    pub guard_units: i32,
    /// Максимальна зміна уставки за один запис.
    pub max_step_units: i32,
}

impl Default for TrimParams {
    fn default() -> Self {
        Self { target_kvar: 12.5, deadband_floor_kvar: 2.5, guard_units: 5, max_step_units: 20 }
    }
}

/// Нова уставка або None, якщо міняти не треба.
///  p_gen_kw    - активна потужність КГУ (має бути >= порога, коли КГУ слухає уставку)
///  v_now       - зчитана з пристрою уставка (не те, що ми колись відправляли)
///  q_grid_kvar - Q на трансформаторі (усереднений), + = недокомпенсація
pub fn trim_setpoint(p_gen_kw: f32, v_now: i32, q_grid_kvar: f32, p: &TrimParams) -> Option<i32> {
    if !(p_gen_kw > 0.0) || !q_grid_kvar.is_finite() {
        return None;
    }
    let v_now = clamp_v(v_now);

    // 1. Похибка відносно цілі
    let err = q_grid_kvar - p.target_kvar;

    // 2. Гістерезис: не менше шуму і не менше половини кванта (інакше крок лише погіршить)
    let band = p.deadband_floor_kvar.max(0.5 * quantum(v_now, p_gen_kw));
    if err.abs() <= band {
        return None;
    }

    // 3. Скільки Q_gen потрібно і яка уставка йому відповідає
    let q_now = q_of(v_now, p_gen_kw);
    let q_want = q_now + err;
    let mut v_new = v_of_q(q_want, p_gen_kw);

    // 4. Не стрибаємо далі за ліміт швидкості, потім обходимо розрив біля 800
    v_new = v_new.clamp(v_now - p.max_step_units, v_now + p.max_step_units);
    v_new = clamp_v(push_out_of_guard(v_new, q_want, p_gen_kw, p.guard_units));
    if v_new == v_now {
        return None;
    }

    // 5. Діємо лише якщо залишок після кроку реально менший за нинішній
    let residual_after = err - (q_of(v_new, p_gen_kw) - q_now);
    if residual_after.abs() >= err.abs() {
        return None;
    }
    Some(v_new)
}


/// Ковзання каретки: простий режим без розрахунку.
/// Q мережі нижче коридору -> уставка +step (КГУ видає менше реактиву, Q мережі росте);
/// Q вище коридору -> уставка -step (КГУ видає більше). Коридор = target +- deadband_floor,
/// за замовчуванням 10..15 квар. Розрив біля 800 перестрибуємо в напрямку руху.
pub fn carriage_step(q_grid_kvar: f32, v_now: i32, p: &TrimParams, step_units: i32) -> Option<i32> {
    if !q_grid_kvar.is_finite() || step_units <= 0 {
        return None;
    }
    let low = p.target_kvar - p.deadband_floor_kvar;
    let high = p.target_kvar + p.deadband_floor_kvar;
    let dir: i32 = if q_grid_kvar < low {
        1
    } else if q_grid_kvar > high {
        -1
    } else {
        return None; // у коридорі
    };

    let v_now = clamp_v(v_now);
    let mut v_new = v_now + dir * step_units;
    if (v_new - V_UNITY).abs() < p.guard_units {
        v_new = if dir > 0 { V_UNITY + p.guard_units } else { V_UNITY - p.guard_units };
    }
    let v_new = clamp_v(v_new);
    if v_new == v_now {
        None // вперлись у межу шкали
    } else {
        Some(v_new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn scale_matches_hand_calculation() {
        assert!(near(q_of(800, 400.0), 0.0, 0.01));
        assert!(near(q_of(799, 400.0), 8.94, 0.05));
        assert!(near(q_of(801, 400.0), -8.94, 0.05));
        assert!(near(q_of(795, 400.0), 20.0, 0.1));
        assert!(near(q_of(795, 1000.0), 50.0, 0.2));
        assert!(near(q_of(600, 400.0), 131.5, 0.5)); // cos 0.95
    }

    #[test]
    fn q_is_strictly_decreasing_in_v() {
        for p in [400.0, 650.0, 1000.0] {
            for v in 0..V_MAX {
                assert!(q_of(v, p) > q_of(v + 1, p), "p={p} v={v}");
            }
        }
    }

    #[test]
    fn roundtrip_for_every_setpoint() {
        for p in [400.0, 520.0, 1000.0] {
            for v in 0..=V_MAX {
                assert_eq!(v_of_q(q_of(v, p), p), v, "p={p} v={v}");
            }
        }
    }

    #[test]
    fn quantum_is_coarse_near_unity_and_fine_in_working_zone() {
        assert!(near(quantum(800, 400.0), 8.94, 0.1));
        assert!(near(quantum(800, 1000.0), 22.4, 0.2));
        assert!(near(quantum(700, 400.0), 0.47, 0.05));
        assert!(quantum(800, 400.0) > 15.0 * quantum(700, 400.0));
    }

    #[test]
    fn undercompensation_lowers_setpoint() {
        // Q_gen = 91 квар при v=700, у мережі +40 квар недокомпенсації -> більше Q_gen -> менше v
        let v = trim_setpoint(400.0, 700, 40.0, &TrimParams::default()).unwrap();
        assert!(v < 700);
    }

    #[test]
    fn overcompensation_raises_setpoint() {
        let v = trim_setpoint(400.0, 700, -40.0, &TrimParams::default()).unwrap();
        assert!(v > 700);
    }

    #[test]
    fn inside_deadband_does_nothing() {
        let p = TrimParams::default();
        // цільові 12.5: шум 2.5 -> коридор 10..15
        assert_eq!(trim_setpoint(400.0, 700, 12.5, &p), None);
        assert_eq!(trim_setpoint(400.0, 700, 14.0, &p), None);
        assert_eq!(trim_setpoint(400.0, 700, 11.0, &p), None);
        assert!(trim_setpoint(400.0, 700, 18.0, &p).is_some());
    }

    #[test]
    fn deadband_widens_with_local_quantum() {
        let p = TrimParams::default();
        // біля одиниці при 1000 кВт квант ~5 квар, половина = 2.6, але вище за шум 2.5
        let q = quantum(795, 1000.0);
        let band = p.deadband_floor_kvar.max(0.5 * q);
        assert!(band > p.deadband_floor_kvar);
        // помилка всередині розширеної смуги -> нічого не робимо
        assert_eq!(trim_setpoint(1000.0, 795, p.target_kvar + band - 0.1, &p), None);
    }

    #[test]
    fn result_never_lands_inside_guard_zone() {
        let p = TrimParams::default();
        for power in [400.0, 600.0, 1000.0] {
            for v_now in (700..=900).step_by(3) {
                for q in [-80.0, -30.0, -5.0, 5.0, 30.0, 80.0] {
                    if let Some(v) = trim_setpoint(power, v_now, q, &p) {
                        assert!((v - V_UNITY).abs() >= p.guard_units, "p={power} v_now={v_now} q={q} -> {v}");
                    }
                }
            }
        }
    }

    #[test]
    fn rate_limit_is_respected() {
        let p = TrimParams::default();
        let v = trim_setpoint(400.0, 700, 300.0, &p).unwrap(); // величезна помилка
        assert_eq!(v, 700 - p.max_step_units);
    }

    #[test]
    fn step_always_improves_the_residual() {
        let p = TrimParams::default();
        for power in [400.0, 700.0, 1000.0] {
            for v_now in (400..=1000).step_by(7) {
                for q in [-120.0, -40.0, -10.0, 0.0, 30.0, 60.0, 150.0] {
                    if let Some(v) = trim_setpoint(power, v_now, q, &p) {
                        let err = q - p.target_kvar;
                        let after = err - (q_of(v, power) - q_of(v_now, power));
                        assert!(after.abs() < err.abs(), "p={power} v_now={v_now} q={q}");
                    }
                }
            }
        }
    }

    #[test]
    fn bad_input_gives_none() {
        let p = TrimParams::default();
        assert_eq!(trim_setpoint(0.0, 700, 50.0, &p), None);
        assert_eq!(trim_setpoint(-5.0, 700, 50.0, &p), None);
        assert_eq!(trim_setpoint(400.0, 700, f32::NAN, &p), None);
    }

    #[test]
    fn park_value_is_outside_guard() {
        assert_eq!(park_value(5), 795);
    }

    #[test]
    fn carriage_moves_one_unit_towards_corridor() {
        let p = TrimParams::default(); // коридор 10..15
        assert_eq!(carriage_step(5.0, 700, &p, 1), Some(701)); // нижче 10: уставку збільшуємо
        assert_eq!(carriage_step(-30.0, 700, &p, 1), Some(701));
        assert_eq!(carriage_step(20.0, 700, &p, 1), Some(699)); // вище 15: зменшуємо
        assert_eq!(carriage_step(10.0, 700, &p, 1), None); // межі коридору включно
        assert_eq!(carriage_step(12.5, 700, &p, 1), None);
        assert_eq!(carriage_step(15.0, 700, &p, 1), None);
    }

    #[test]
    fn carriage_jumps_over_guard_zone_in_direction_of_motion() {
        let p = TrimParams::default(); // guard 5: зона 796..804
        assert_eq!(carriage_step(0.0, 795, &p, 1), Some(805)); // вгору: 796 у зоні -> 805
        assert_eq!(carriage_step(30.0, 805, &p, 1), Some(795)); // вниз: 804 у зоні -> 795
        assert_eq!(carriage_step(0.0, 802, &p, 1), Some(805)); // ми всередині зони, їдемо вгору
        assert_eq!(carriage_step(30.0, 797, &p, 1), Some(795)); // всередині зони, їдемо вниз
    }

    #[test]
    fn carriage_stops_at_scale_limits_and_rejects_bad_input() {
        let p = TrimParams::default();
        assert_eq!(carriage_step(0.0, V_MAX, &p, 1), None);
        assert_eq!(carriage_step(30.0, 0, &p, 1), None);
        assert_eq!(carriage_step(f32::NAN, 700, &p, 1), None);
        assert_eq!(carriage_step(0.0, 700, &p, 0), None);
        assert_eq!(carriage_step(0.0, 700, &p, 5), Some(705));
    }

    #[test]
    fn carriage_result_never_lands_inside_guard_zone() {
        let p = TrimParams::default();
        for v_now in 0..=V_MAX {
            for q in [-50.0, 0.0, 30.0, 90.0] {
                for step in [1, 3] {
                    if let Some(v) = carriage_step(q, v_now, &p, step) {
                        assert!((v - V_UNITY).abs() >= p.guard_units, "v_now={v_now} q={q} -> {v}");
                    }
                }
            }
        }
    }
}