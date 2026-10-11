//! 隨機源抽象。
//!
//! - 正式路徑：[`CryptoRng`] —— `rand::rngs::ThreadRng`（ChaCha12，OS 熵源播種，
//!   密碼學安全）。每顆骰子不跨 GIL。
//! - 注入路徑：[`PyCallbackRng`] —— 測試把 `random.Random(seed)` 傳進來時，
//!   直接呼叫它的 `randint`，讓既有 100+ 測試的決定性 oracle 維持不變。

/// 骰子只需要「閉區間整數」這一種隨機操作。
pub trait DiceRng {
    /// 回傳 `[lo, hi]` 內均勻分布的整數。
    fn randint(&mut self, lo: i64, hi: i64) -> i64;
}

/// 正式路徑：ChaCha12-based CSPRNG（`rand::rngs::ThreadRng`）。
pub struct CryptoRng {
    inner: rand::rngs::ThreadRng,
}

impl CryptoRng {
    pub fn new() -> Self {
        Self {
            inner: rand::rngs::ThreadRng::default(),
        }
    }
}

impl Default for CryptoRng {
    fn default() -> Self {
        Self::new()
    }
}

impl DiceRng for CryptoRng {
    #[inline]
    fn randint(&mut self, lo: i64, hi: i64) -> i64 {
        use rand::RngExt;
        self.inner.random_range(lo..=hi)
    }
}

/// 決定性測試用：小型的 splitmix64，不依賴 OS 熵源。
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self {
            // 避免 seed=0 時全零狀態
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl DiceRng for SeededRng {
    #[inline]
    fn randint(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo + 1) as u64;
        debug_assert!(span > 0);
        lo + (self.next_u64() % span) as i64
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_rng_stays_in_range() {
        let mut rng = CryptoRng::new();
        for _ in 0..1000 {
            let v = rng.randint(1, 6);
            assert!((1..=6).contains(&v), "out of range: {v}");
        }
        assert!(rng.randint(0, 1) <= 1);
        assert_eq!(rng.randint(1, 1), 1);
    }

    #[test]
    fn crypto_rng_is_not_constant() {
        let mut rng = CryptoRng::new();
        let vals: Vec<i64> = (0..64).map(|_| rng.randint(1, 100)).collect();
        assert!(vals.iter().any(|v| *v != vals[0]));
    }

    #[test]
    fn seeded_rng_is_reproducible() {
        let a: Vec<i64> = (0..32).map(|_| SeededRng::new(42).randint(1, 6)).collect();
        assert!(a.iter().all(|v| (1..=6).contains(v)));
        let mut r1 = SeededRng::new(7);
        let mut r2 = SeededRng::new(7);
        for _ in 0..64 {
            assert_eq!(r1.randint(1, 20), r2.randint(1, 20));
        }
    }
}
