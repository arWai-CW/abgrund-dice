use rolling_rs::engine::roll;
use rolling_rs::rng::{CryptoRng, DiceRng, SeededRng};
use std::time::Instant;

fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) {
    for _ in 0..iters / 10 + 1 {
        f();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        f();
    }
    let us = t0.elapsed().as_secs_f64() * 1e6 / iters as f64;
    println!("{label:44} {us:9.3} us");
}

fn main() {
    // 1) 每顆骰子只做 RNG，不建構任何東西
    time("rng only: CryptoRng 100 draws", 2000, || {
        let mut r = CryptoRng::new();
        let mut s = 0i64;
        for _ in 0..100 {
            s += r.randint(1, 6);
        }
        std::hint::black_box(s);
    });
    time("rng only: SeededRng 100 draws", 2000, || {
        let mut r = SeededRng::new(1);
        let mut s = 0i64;
        for _ in 0..100 {
            s += r.randint(1, 6);
        }
        std::hint::black_box(s);
    });
    // 2) reuse 同一顆 CryptoRng（隔離 ThreadRng::default() 成本）
    {
        let mut r = CryptoRng::new();
        time("rng only: reused CryptoRng 100 draws", 2000, || {
            let mut s = 0i64;
            for _ in 0..100 {
                s += r.randint(1, 6);
            }
            std::hint::black_box(s);
        });
    }
    // 3) 完整 roll（含 render）
    time("roll(100d6) CryptoRng", 1000, || {
        std::hint::black_box(roll("100d6", &mut CryptoRng::new()).unwrap());
    });
    time("roll(100d6) SeededRng", 1000, || {
        std::hint::black_box(roll("100d6", &mut SeededRng::new(1)).unwrap());
    });
    // 4) 小公式
    time("roll(4d6k3) CryptoRng", 20_000, || {
        std::hint::black_box(roll("4d6k3", &mut CryptoRng::new()).unwrap());
    });
    time("roll(4d6k3+2d8kh+1d20>=15) CryptoRng", 20_000, || {
        std::hint::black_box(roll("4d6k3+2d8kh+1d20>=15", &mut CryptoRng::new()).unwrap());
    });
}
