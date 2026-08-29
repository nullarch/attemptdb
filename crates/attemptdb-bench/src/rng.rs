//! Deterministic pseudo-random numbers for the workload generator.
//!
//! xoshiro256** seeded through SplitMix64. Kept in-tree so the workload is
//! reproducible from a seed alone and the crate pulls in no RNG dependency.

use crate::model::Quantiles;

#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut st = seed;
        Self {
            s: [
                splitmix(&mut st),
                splitmix(&mut st),
                splitmix(&mut st),
                splitmix(&mut st),
            ],
        }
    }

    /// An independent child stream (for per-session generators).
    pub fn fork(&mut self) -> Rng {
        Rng::new(self.next_u64())
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform integer in `[0, n)`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        ((self.f64() * n as f64) as u64).min(n - 1)
    }

    /// Uniform integer in `[lo, hi]`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// Uniform choice from a list of static strings.
    pub fn word(&mut self, items: &[&'static str]) -> &'static str {
        items[self.below(items.len() as u64) as usize]
    }

    /// Weighted choice; weights need not sum to one.
    pub fn weighted<'a, T>(&mut self, items: &'a [(T, f64)]) -> &'a T {
        let total: f64 = items.iter().map(|(_, w)| w).sum();
        let mut x = self.f64() * total;
        for (item, w) in items {
            if x < *w {
                return item;
            }
            x -= w;
        }
        &items[items.len() - 1].0
    }

    /// Draw from an empirical quantile table.
    pub fn sample(&mut self, q: &Quantiles) -> f64 {
        let u = self.f64();
        q.sample(u)
    }

    /// Draw a byte length from a quantile table.
    pub fn len(&mut self, q: &Quantiles) -> usize {
        self.sample(q).round().max(0.0) as usize
    }

    /// Lowercase hex string of `n` characters.
    pub fn hex(&mut self, n: usize) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(n);
        let mut bits = self.next_u64();
        let mut left = 16;
        for _ in 0..n {
            if left == 0 {
                bits = self.next_u64();
                left = 16;
            }
            s.push(DIGITS[(bits & 0xf) as usize] as char);
            bits >>= 4;
            left -= 1;
        }
        s
    }

    /// A UUID-shaped identifier (random version-4 layout, seeded).
    pub fn uuid_like(&mut self) -> String {
        format!(
            "{}-{}-4{}-{}-{}",
            self.hex(8),
            self.hex(4),
            self.hex(3),
            self.hex(4),
            self.hex(12)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn ranges_are_inclusive_and_bounded() {
        let mut r = Rng::new(1);
        for _ in 0..10_000 {
            let v = r.range(3, 5);
            assert!((3..=5).contains(&v));
            assert!(r.below(1) == 0);
            let f = r.f64();
            assert!((0.0..1.0).contains(&f));
        }
    }
}
