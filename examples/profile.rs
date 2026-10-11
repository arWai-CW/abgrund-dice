// 拆解 46 組爆炸池的 1.444ms 到底花在哪裡
use rolling_rs::engine::roll;
use rolling_rs::rng::{CryptoRng, DiceRng};
use std::time::Instant;

const GROUPS: usize = 46;
const PER_GROUP: usize = 1000;

fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) {
    for _ in 0..iters / 5 + 1 { f(); }
    let t0 = Instant::now();
    for _ in 0..iters { f(); }
    let ms = t0.elapsed().as_secs_f64() * 1e3 / iters as f64;
    println!("{label:48} {ms:8.3} ms");
}

fn main() {
    let formula = vec!["100d6x<=6"; GROUPS].join("+");
    println!("公式 {} 字元, {} 組 × {} 顆 = {} 顆骰\n",
        formula.len(), GROUPS, PER_GROUP, GROUPS * PER_GROUP);

    let total = GROUPS * PER_GROUP;

    // (a) 只有 RNG
    time("(a) RNG only: 46000 draws", 20, || {
        let mut r = CryptoRng::new();
        let mut acc = 0i64;
        for _ in 0..total { acc = acc.wrapping_add(r.randint(1, 6)); }
        std::hint::black_box(acc);
    });

    // (b) RNG + Vec<DieRoll> 建構（無修飾詞、無渲染）
    time("(b) RNG + Vec<DieRoll> push", 20, || {
        let mut r = CryptoRng::new();
        let mut v: Vec<(i64, bool)> = Vec::with_capacity(total);
        for _ in 0..total { v.push((r.randint(1, 6), false)); }
        std::hint::black_box(v.len());
    });

    // (c) 字串建構：46000 個小 String
    time("(c) String building: 46000 small strings", 20, || {
        let mut v: Vec<String> = Vec::with_capacity(total);
        for i in 0..total { v.push((i % 1000).to_string()); }
        std::hint::black_box(v.len());
    });

    // (d) 每組 join 1000 個字串 + 外層再 join 46 個
    let parts: Vec<String> = (0..PER_GROUP).map(|i| (i % 1000).to_string()).collect();
    time("(d) join 1000 + outer join 46 x46", 20, || {
        let inner: Vec<String> = (0..GROUPS).map(|_| parts.join("+")).collect();
        std::hint::black_box(inner.join(", ").len());
    });

    // (e) 完整 roll
    time("(e) FULL roll() 46 groups", 20, || {
        std::hint::black_box(roll(&formula, &mut CryptoRng::new()).unwrap());
    });

    // (f) 完整 roll，但每組單獨呼叫（= 可並行的工作單位大小）
    let one = "100d6x<=6";
    time("(f) roll(100d6x<=6) x46 — 單組大小", 20, || {
        let mut n = 0usize;
        for _ in 0..GROUPS {
            n += roll(one, &mut CryptoRng::new()).unwrap().groups[0].rolls.len();
        }
        std::hint::black_box(n);
    });
}
