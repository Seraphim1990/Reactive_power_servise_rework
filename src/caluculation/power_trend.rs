use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    Stable,
    PowerUp,
    PowerDown,
}

pub struct PowerTrendTracker {
    buf: VecDeque<f32>,
    capacity: usize,
    noise_threshold: f32,
}

impl PowerTrendTracker {
    pub fn new(capacity: usize, noise_threshold: f32) -> Self {
        assert!(
            capacity >= 3,
            "trend window must contain at least 3 samples"
        );
        assert!(noise_threshold.is_finite() && noise_threshold >= 0.0);
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            noise_threshold,
        }
    }

    pub fn push(&mut self, power: f32) -> Trend {
        if !power.is_finite() {
            return Trend::Stable;
        }
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(power);
        self.evaluate()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn evaluate(&self) -> Trend {
        if self.buf.len() < 3 {
            return Trend::Stable;
        }
        let (mut up, mut down) = (false, false);
        for i in 1..self.buf.len() {
            let diff = self.buf[i] - self.buf[i - 1];
            if diff == 0.0 || diff.abs() < self.noise_threshold {
                continue;
            }
            up |= diff > 0.0;
            down |= diff < 0.0;
        }
        match (up, down) {
            (true, false) => Trend::PowerUp,
            (false, true) => Trend::PowerDown,
            _ => Trend::Stable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direction_noise_reversal_and_clear() {
        let mut t = PowerTrendTracker::new(3, 2.0);
        assert_eq!(t.push(390.0), Trend::Stable);
        assert_eq!(t.push(395.0), Trend::Stable);
        assert_eq!(t.push(400.0), Trend::PowerUp);
        assert_eq!(t.push(396.0), Trend::Stable);
        assert_eq!(t.push(390.0), Trend::PowerDown);
        t.clear();
        for p in [400.0, 400.5, 401.0] {
            assert_eq!(t.push(p), Trend::Stable);
        }
        assert_eq!(t.push(f32::NAN), Trend::Stable);
    }
}
