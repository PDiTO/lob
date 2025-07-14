//! A small, seedable PRNG (xoshiro256++ seeded through SplitMix64).
//!
//! Written out here rather than pulled from `rand` so that a seed means the same
//! stream forever, independent of dependency upgrades. The simulator's determinism
//! guarantee rests on this.

#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        let mut next = || {
            sm = sm.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = sm;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in [0, n). `n` must be non-zero.
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        // Lemire's multiply-shift; the bias for the ranges used here is negligible.
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in [lo, hi].
    #[inline]
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    /// Exponential with the given mean.
    #[inline]
    pub fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.f64()).ln()
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.f64();
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn distributions_have_roughly_the_right_moments() {
        let mut r = Rng::new(7);
        let n = 200_000;
        let (mut u, mut e, mut z, mut z2) = (0.0, 0.0, 0.0, 0.0);
        for _ in 0..n {
            u += r.f64();
            e += r.exp(3.0);
            let x = r.normal();
            z += x;
            z2 += x * x;
        }
        let n = n as f64;
        assert!((u / n - 0.5).abs() < 0.01);
        assert!((e / n - 3.0).abs() < 0.05);
        assert!((z / n).abs() < 0.01);
        assert!((z2 / n - 1.0).abs() < 0.02);
    }

    #[test]
    fn below_stays_in_range() {
        let mut r = Rng::new(3);
        let mut seen = [false; 5];
        for _ in 0..1000 {
            seen[r.below(5) as usize] = true;
            let x = r.range(10, 12);
            assert!((10..=12).contains(&x));
        }
        assert!(seen.iter().all(|&s| s));
    }
}
