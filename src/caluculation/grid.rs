//! Transformer measurements only. Every decision consumes a fresh P/Q pair.
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    Grid,
    Island,
    Blackout,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GridConfig {
    pub island_max: f64,
    #[serde(with = "super::settings::seconds")]
    pub island_hold: Duration,
    pub island_min_samples: u32,
    #[serde(with = "super::settings::seconds")]
    pub analyzer_timeout: Duration,
    #[serde(with = "super::settings::seconds")]
    pub off_retry: Duration,
}
impl Default for GridConfig {
    fn default() -> Self {
        Self {
            island_max: 15.0,
            island_hold: Duration::from_secs(10),
            island_min_samples: 5,
            analyzer_timeout: Duration::from_secs(10),
            off_retry: Duration::from_secs(5),
        }
    }
}
pub struct GridMonitor {
    pub cfg: GridConfig,
    mode: GridMode,
    started: Instant,
    p: Option<(f64, Instant)>,
    q: Option<(f64, Instant)>,
    p_new: bool,
    q_new: bool,
    low_since: Option<Instant>,
    samples: u32,
}
impl GridMonitor {
    pub fn new(cfg: GridConfig, now: Instant) -> Self {
        Self {
            cfg,
            mode: GridMode::Grid,
            started: now,
            p: None,
            q: None,
            p_new: false,
            q_new: false,
            low_since: None,
            samples: 0,
        }
    }
    pub fn mode(&self) -> GridMode {
        self.mode
    }
    pub fn analyzer_ages(&self, now: Instant) -> (Option<Duration>, Option<Duration>) {
        (
            self.p.map(|(_, at)| now.saturating_duration_since(at)),
            self.q.map(|(_, at)| now.saturating_duration_since(at)),
        )
    }

    pub fn power(&mut self, now: Instant, value: f64) {
        self.update(now, value, true);
    }
    pub fn reactive(&mut self, now: Instant, value: f64) {
        self.update(now, value, false);
    }
    fn update(&mut self, now: Instant, value: f64, active: bool) {
        self.tick(now);
        if !value.is_finite() {
            return;
        }
        if active {
            self.p = Some((value, now));
            self.p_new = true;
        } else {
            self.q = Some((value, now));
            self.q_new = true;
        }
        if !(0.0..=self.cfg.island_max).contains(&value) {
            // A valid non-island reading proves the analyzer is responding again.
            if self.mode == GridMode::Blackout {
                self.mode = GridMode::Grid;
            }
            self.low_since = None;
            self.samples = 0;
        }
        if !self.p_new || !self.q_new {
            return;
        }
        let (p, q) = (self.p.unwrap(), self.q.unwrap());
        if now.saturating_duration_since(p.1) >= self.cfg.analyzer_timeout
            || now.saturating_duration_since(q.1) >= self.cfg.analyzer_timeout
        {
            return;
        }
        self.p_new = false;
        self.q_new = false;
        if (0.0..=self.cfg.island_max).contains(&p.0) && (0.0..=self.cfg.island_max).contains(&q.0)
        {
            let since = *self.low_since.get_or_insert(now);
            self.samples = self.samples.saturating_add(1);
            if self.samples >= self.cfg.island_min_samples
                && now.saturating_duration_since(since) >= self.cfg.island_hold
            {
                self.mode = GridMode::Island;
            }
            // After blackout, low readings must qualify again before enabling anything.
        } else {
            self.low_since = None;
            self.samples = 0;
            self.mode = GridMode::Grid;
        }
    }
    pub fn tick(&mut self, now: Instant) {
        let stale = |sample: Option<(f64, Instant)>| {
            now.saturating_duration_since(sample.map_or(self.started, |s| s.1))
                >= self.cfg.analyzer_timeout
        };
        // Blackout means no measurements from the analyzer, not one quiet tag.
        if stale(self.p) && stale(self.q) {
            if self.mode != GridMode::Blackout {
                self.p_new = false;
                self.q_new = false;
            }
            self.mode = GridMode::Blackout;
            self.low_since = None;
            self.samples = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair(m: &mut GridMonitor, now: Instant, p: f64, q: f64) {
        m.power(now, p);
        m.reactive(now, q);
    }
    #[test]
    fn requires_ten_seconds_and_fresh_pairs() {
        let now = Instant::now();
        let mut m = GridMonitor::new(GridConfig::default(), now);
        for i in 0..5 {
            pair(&mut m, now + Duration::from_secs(i * 2), 15.0, 0.0);
        }
        assert_eq!(m.mode(), GridMode::Grid);
        pair(&mut m, now + Duration::from_secs(10), 0.0, 15.0);
        assert_eq!(m.mode(), GridMode::Island);
        pair(&mut m, now + Duration::from_secs(12), 16.0, 5.0);
        assert_eq!(m.mode(), GridMode::Grid);
    }
    #[test]
    fn silence_invalid_values_and_single_channel_cannot_confirm_island() {
        let now = Instant::now();
        let mut m = GridMonitor::new(GridConfig::default(), now);
        pair(&mut m, now, 5.0, 5.0);
        for i in 1..=10 {
            m.power(now + Duration::from_secs(i), 5.0);
            m.reactive(now + Duration::from_secs(i), f64::NAN);
        }
        assert_eq!(m.mode(), GridMode::Grid); // P is still arriving; no island without fresh Q.
        m.tick(now + Duration::from_secs(20));
        assert_eq!(m.mode(), GridMode::Blackout);
        pair(&mut m, now + Duration::from_secs(21), 40.0, 20.0);
        assert_eq!(m.mode(), GridMode::Grid);
    }
    #[test]
    fn unchanged_p_does_not_cause_blackout_while_q_keeps_arriving() {
        let now = Instant::now();
        let mut m = GridMonitor::new(GridConfig::default(), now);
        pair(&mut m, now, 100.0, 170.0);
        for seconds in (2..=40).step_by(2) {
            let at = now + Duration::from_secs(seconds);
            m.tick(at);
            m.reactive(at, 170.0);
            assert_eq!(m.mode(), GridMode::Grid);
        }
        m.tick(now + Duration::from_secs(50));
        assert_eq!(m.mode(), GridMode::Blackout);
        m.reactive(now + Duration::from_secs(51), 170.0);
        assert_eq!(m.mode(), GridMode::Grid);
    }

    #[test]
    fn out_of_range_resets_window_and_blackout_overrides_island() {
        let now = Instant::now();
        let mut m = GridMonitor::new(GridConfig::default(), now);
        for i in 0..=5 {
            pair(&mut m, now + Duration::from_secs(i * 2), 5.0, 5.0);
        }
        assert_eq!(m.mode(), GridMode::Island);
        m.tick(now + Duration::from_secs(20));
        assert_eq!(m.mode(), GridMode::Blackout);
        for i in 0..5 {
            pair(&mut m, now + Duration::from_secs(21 + i * 2), 5.0, 5.0);
        }
        assert_eq!(m.mode(), GridMode::Blackout);
        pair(&mut m, now + Duration::from_secs(31), 5.0, 5.0);
        assert_eq!(m.mode(), GridMode::Island);
    }
}
