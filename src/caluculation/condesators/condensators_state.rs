use serde::{Deserialize, Serialize};
use std::time::Duration;
use std::{fmt, fs, io, path::Path};

pub const MAX_BANKS: usize = 16;

/// Уставка = коефіцієнт * мінімальний щабель. Константа, у файл не виносимо.
/// Знак «+» = трохи недокомпенсації (реактив тече до заводу).
pub const CAP_SETPOINT_STEPS: f32 = 1.5;

/// Стан актора конденсаторних батарей.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CondensatorState {
    /// Номінали щаблів, квар. Індекс = номер біта реле. 0 = щабля нема (у арифметику не входить).
    pub banks_kvar: Vec<f32>,
    /// Коефіцієнт трансформації: V_банок = V_10кВ / k. Змінив анцапфу - змінив k.
    pub transformer_ratio: f32,
    /// З паспорта БАТАРЕЙ: мінімальний час між вимкненням і наступним вмиканням (розряд), с.
    /// По одному значенню на щабель, індекс той самий, що в banks_kvar.
    pub min_off_secs: Vec<f32>,
    /// З паспорта ПУСКАЧІВ: мінімальний час у ввімкненому стані, с.
    pub min_on_secs: f32,
    /// Запас до паспортних значень: 0.05 = +5%.
    pub safety_margin: f32,
    /// Період опитування зворотного зв'язку, с. Додається до ембарго (лаг зворотного зв'язку).
    pub poll_period_secs: f32,

    pub bank_voltage_nom: f32,
}

#[derive(Debug)]
pub enum CondensatorError {
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Invalid(String),
}

impl fmt::Display for CondensatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Parse(e) => write!(f, "parse: {e}"),
            Self::Serialize(e) => write!(f, "serialize: {e}"),
            Self::Invalid(s) => write!(f, "invalid state: {s}"),
        }
    }
}
impl std::error::Error for CondensatorError {}
impl From<io::Error> for CondensatorError { fn from(e: io::Error) -> Self { Self::Io(e) } }
impl From<toml::de::Error> for CondensatorError { fn from(e: toml::de::Error) -> Self { Self::Parse(e) } }
impl From<toml::ser::Error> for CondensatorError { fn from(e: toml::ser::Error) -> Self { Self::Serialize(e) } }

impl Default for CondensatorState {
    fn default() -> Self {
        // Заглушка: підстав свої реальні значення в condensators.toml
        Self {
            banks_kvar: vec![10.0, 25.0, 25.0, 40.0, 40.0, 50.0, 50.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            transformer_ratio: 10600.0 / 400.0,
            min_off_secs: vec![60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0], // 60 с, поки не підтверджено паспортом (енергетик казав 30)
            min_on_secs: 30.0,
            safety_margin: 0.05,
            poll_period_secs: 2.0,
            bank_voltage_nom: 400.0
        }
    }
}

impl CondensatorState {
    pub fn validate(&self) -> Result<(), CondensatorError> {
        let bad = |s: &str| Err(CondensatorError::Invalid(s.to_string()));
        if self.banks_kvar.len() > MAX_BANKS {
            return bad("більше 16 щаблів (маска реле - u16)");
        }
        if self.banks_kvar.iter().any(|c| !c.is_finite() || *c < 0.0) {
            return bad("номінал щабля має бути скінченним і >= 0");
        }
        if self.banks_kvar.iter().all(|c| *c == 0.0) {
            return bad("немає жодного активного щабля");
        }
        if !self.transformer_ratio.is_finite() || self.transformer_ratio <= 0.0 {
            return bad("transformer_ratio має бути > 0");
        }
        if self.min_off_secs.len() != self.banks_kvar.len() {
            return bad("min_off_secs має бути такої самої довжини, як banks_kvar");
        }
        if self.min_off_secs.iter().any(|t| !t.is_finite() || *t < 0.0) {
            return bad("min_off_secs: значення мають бути скінченними і >= 0");
        }
        if !self.min_on_secs.is_finite() || self.min_on_secs < 0.0 {
            return bad("min_on_secs має бути >= 0");
        }
        if !self.safety_margin.is_finite() || !(0.0..=0.5).contains(&self.safety_margin) {
            return bad("safety_margin має бути в діапазоні 0..=0.5 (0.05 = 5%)");
        }
        if !self.poll_period_secs.is_finite() || !(0.0..=60.0).contains(&self.poll_period_secs) {
            return bad("poll_period_secs має бути в діапазоні 0..=60");
        }
        Ok(())
    }

    /// Найменший активний щабель, квар (нулі ігноруються).
    pub fn min_step(&self) -> f32 {
        self.banks_kvar.iter().copied().filter(|c| *c > 0.0).fold(f32::INFINITY, f32::min)
    }

    pub fn setpoint(&self) -> f32 {
        CAP_SETPOINT_STEPS * self.min_step()
    }

    /// Напруга на шинах банок з виміру на стороні 10 кВ.
    pub fn bank_voltage(&self, v_10kv: f32) -> f32 {
        v_10kv / self.transformer_ratio
    }

    /// Ефективне жорстке ембарго на вмикання: паспорт * (1 + запас) + період опитування.
    pub fn effective_min_off(&self, bank: usize) -> Duration {
        let secs = self.min_off_secs[bank] * (1.0 + self.safety_margin) + self.poll_period_secs;
        Duration::from_secs_f32(secs)
    }

    /// Ефективне м'яке ембарго на вимкнення: паспорт * (1 + запас) + період опитування.
    pub fn effective_min_on(&self) -> Duration {
        let secs = self.min_on_secs * (1.0 + self.safety_margin) + self.poll_period_secs;
        Duration::from_secs_f32(secs)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, CondensatorError> {
        let s: Self = toml::from_str(&fs::read_to_string(path)?)?;
        s.validate()?;
        Ok(s)
    }

    /// Якщо файлу нема - створює його зі значеннями за замовчуванням.
    /// Якщо файл є, але зіпсований - помилка (мовчки перезаписувати не можна).
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, CondensatorError> {
        let path = path.as_ref();
        match Self::load(path) {
            Err(CondensatorError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                let s = Self::default();
                s.save(path)?;
                Ok(s)
            }
            other => other,
        }
    }

    /// Атомарний запис: tmp-файл + rename, щоб обрив живлення не залишив половину файлу.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CondensatorError> {
        self.validate()?;
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, toml::to_string_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("kvar_ctl_{}_{name}", std::process::id()))
            .join("condensators.toml")
    }

    fn st(banks: Vec<f32>, k: f32) -> CondensatorState {
        let n = banks.len();
        CondensatorState {
            banks_kvar: banks,
            transformer_ratio: k,
            min_off_secs: vec![60.0; n],
            min_on_secs: 30.0,
            safety_margin: 0.05,
            poll_period_secs: 2.0,
            bank_voltage_nom: 400.0
        }
    }

    #[test]
    fn roundtrip() {
        let p = tmp("rt");
        let s = st(vec![0.0, 12.5, 25.0], 28.5);
        s.save(&p).unwrap();
        assert_eq!(CondensatorState::load(&p).unwrap(), s);
    }

    #[test]
    fn creates_default_when_missing() {
        let p = tmp("def");
        let s = CondensatorState::load_or_create(&p).unwrap();
        assert_eq!(s, CondensatorState::default());
        assert!(p.exists());
    }

    #[test]
    fn setpoint_ignores_zero_banks() {
        let s = st(vec![0.0, 20.0, 40.0], 1.0);
        assert_eq!(s.min_step(), 20.0);
        assert_eq!(s.setpoint(), 30.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(st(vec![0.0], 1.0).validate().is_err());
        assert!(st(vec![10.0], 0.0).validate().is_err());
        assert!(st(vec![1.0; 17], 1.0).validate().is_err());
        assert!(st(vec![-5.0, 10.0], 1.0).validate().is_err());
        assert!(st(vec![f32::NAN, 10.0], 1.0).validate().is_err());

        let mut s = st(vec![10.0, 20.0], 1.0);
        s.min_off_secs = vec![60.0]; // не збігається з banks_kvar
        assert!(s.validate().is_err());

        let mut s = st(vec![10.0], 1.0);
        s.safety_margin = 5.0; // друкарська помилка: 5 замість 0.05
        assert!(s.validate().is_err());
    }

    #[test]
    fn effective_embargo_is_passport_plus_margin_plus_poll() {
        let s = st(vec![10.0], 1.0);
        // 60 * 1.05 + 2 = 65
        assert!((s.effective_min_off(0).as_secs_f32() - 65.0).abs() < 0.01);
        // 30 * 1.05 + 2 = 33.5
        assert!((s.effective_min_on().as_secs_f32() - 33.5).abs() < 0.01);
    }

    #[test]
    fn corrupted_file_is_not_overwritten() {
        let p = tmp("bad");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "banks_kvar = [").unwrap();
        assert!(CondensatorState::load_or_create(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "banks_kvar = [");
    }
}
