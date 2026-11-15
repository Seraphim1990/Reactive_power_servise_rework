//! Кільцевий буфер фіксованого розміру: без алокацій, без бібліотек.

#[derive(Debug, Clone)]
pub struct RingBuf<const N: usize> {
    data: [f32; N],
    head: usize, // куди писатимемо наступний відлік
    len: usize,  // скільки відліків реально накопичено (до N)
}

impl<const N: usize> RingBuf<N> {
    pub fn new() -> Self {
        Self { data: [0.0; N], head: 0, len: 0 }
    }

    pub fn push(&mut self, v: f32) {
        self.data[self.head] = v;
        self.head = (self.head + 1) % N;
        if self.len < N {
            self.len += 1;
        }
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Відлік з кінця: 0 = найновіший, 1 = попередній. None, якщо стільки ще нема.
    pub fn get_back(&self, back: usize) -> Option<f32> {
        if back >= self.len {
            return None;
        }
        let idx = (self.head + N - 1 - back) % N;
        Some(self.data[idx])
    }

    /// Середнє за останні k відліків. None, якщо їх менше за k (нема даних != нуль).
    pub fn mean_last(&self, k: usize) -> Option<f32> {
        if k == 0 || k > N || self.len < k {
            return None;
        }
        let mut sum = 0.0;
        for back in 0..k {
            sum += self.data[(self.head + N - 1 - back) % N];
        }
        Some(sum / k as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_until_filled() {
        let mut b = RingBuf::<4>::new();
        b.push(1.0);
        b.push(2.0);
        assert_eq!(b.mean_last(4), None);
        assert_eq!(b.mean_last(2), Some(1.5));
        assert_eq!(b.get_back(0), Some(2.0));
        assert_eq!(b.get_back(2), None);
    }

    #[test]
    fn wraps_and_keeps_latest() {
        let mut b = RingBuf::<3>::new();
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            b.push(v);
        }
        assert_eq!(b.len(), 3);
        assert_eq!(b.mean_last(3), Some(4.0)); // 3,4,5
        assert_eq!(b.get_back(0), Some(5.0));
        assert_eq!(b.get_back(2), Some(3.0));
    }

    #[test]
    fn clear_resets() {
        let mut b = RingBuf::<3>::new();
        b.push(1.0);
        b.clear();
        assert_eq!(b.len(), 0);
        assert_eq!(b.mean_last(1), None);
    }
}
