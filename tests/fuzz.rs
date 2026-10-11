//! 模糊測試：`roll()` 對任意輸入**永不 panic**，且錯誤一定是 `錯誤：…`。
//!
//! 為什麼這在 Rust 特別重要：舊 Python 遇到壞輸入只會拋 `RollError`；
//! Rust 端的 panic 會變成 PyO3 的 `PanicException`，而它是 `BaseException`
//! 的直系子類——`cogs/diceRoll.py` 的 `except RollError` **攔不到**，
//! 會變成未處理例外。所以「不 panic」是這個 extension 的對外契約。
//!
//! 三種產生器：
//! 1. **文法感知**（`gen_formula`）——依產生式組出多半合法的公式，把隨機度
//!    花在解析器與評估器的深處，而不是在 lexer 就全滅。
//! 2. **突變**——對合法公式做刪除 / 重複 / 換位 / 插入，製造「差一點就合法」。
//! 3. **純亂碼**——含非 ASCII 與控制字元，逼 lexer 的 `Err` 分支。
//!
//! 決定性（固定 seed），CI 可重現。

use rolling_rs::engine::{format_result, roll};
use rolling_rs::error::RollError;
use rolling_rs::rng::SeededRng;
use rolling_rs::types::RollResult;

/// xorshift64*：夠用、無依賴、決定性。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

// ------------------------------------------------------- 文法感知產生器

const DICE_MODS: &[&str] = &[
    "", "", "", "k", "k3", "kh", "kh2", "kl", "kl0", "dh", "dl", "d", "d2", "d99",
    "r", "rr", "r2", "r<3", "r>=6", "rr>=1", "rr<3", "r2r4", "x", "xo", "x0",
    "x5", "x>=9", "xo10", "x5=10", "x1001", "min", "min2", "max", "max8",
    "min100001", "cs", "cf", "cs>=4", "cf>10", "cs6", "even", "odd", "df", "df1",
    "sf", "sf<3", "ms", "ms10", "ms0", "ms100001", "x=20kh", "k3cs>=4",
    "min2max8", "cs6df1", "rr<3cs>=5", "kh2.5", "min2.5",
];

const COUNTS: &[&str] = &["", "1", "2", "4", "10", "100", "0", "101", "100000"];
const FACES: &[&str] = &["4", "6", "8", "20", "100", "1", "0", "100000", "100001", "-4"];

fn gen_mod(rng: &mut Rng) -> String {
    rng.pick(DICE_MODS).to_string()
}

fn gen_dice(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => format!("{}df{}", rng.pick(COUNTS), ""),
        1 => format!("{}dc{}", rng.pick(COUNTS), rng.pick(&["", "c0", "c1", "cc0", "cc1"])),
        2 => format!("d{}", rng.pick(FACES)),
        3 => format!("1d({})", gen_expr(rng, 0)),
        _ => {
            let mut s = format!("{}d{}", rng.pick(COUNTS), rng.pick(FACES));
            // 疊幾個修飾詞（順序隨機，執行順序由引擎決定）
            for _ in 0..rng.below(3) {
                s.push_str(&gen_mod(rng));
            }
            s
        }
    }
}

fn gen_expr(rng: &mut Rng, depth: usize) -> String {
    if depth >= 3 || rng.chance(3) {
        return match rng.below(4) {
            0 => gen_dice(rng),
            1 => rng.pick(&["1", "2", "10", "0", "2.7", "9999999999999999"]).to_string(),
            2 => format!("{}d{}", rng.pick(COUNTS), rng.pick(FACES)),
            _ => gen_pool(rng, depth),
        };
    }
    match rng.below(5) {
        0 => format!("({})", gen_expr(rng, depth + 1)),
        1 => format!("-{}", gen_expr(rng, depth + 1)),
        2 => format!(
            "{}{}{}",
            gen_expr(rng, depth + 1),
            rng.pick(&["+", "-", "*", "/"]),
            gen_expr(rng, depth + 1)
        ),
        3 => format!(
            "{}({})",
            rng.pick(&["floor", "ceil", "round", "abs"]),
            gen_expr(rng, depth + 1)
        ),
        _ => gen_pool(rng, depth + 1),
    }
}

fn gen_pool(rng: &mut Rng, depth: usize) -> String {
    let n = 1 + rng.below(4);
    let terms: Vec<String> = (0..n).map(|_| gen_expr(rng, depth + 1)).collect();
    let sep = if rng.chance(2) { "," } else { ", " };
    format!(
        "{{{}}}{}",
        terms.join(sep),
        rng.pick(&["", "", "kh", "kh2", "kl", "dl", "d", "cs", "cs>15", "cf<=15", "even", "odd"])
    )
}

fn gen_formula(rng: &mut Rng) -> String {
    let mut f = gen_expr(rng, 0);
    if rng.chance(3) {
        f.push_str(rng.pick(&[">=", ">", "<=", "<", "="]));
        f.push_str(rng.pick(&["1", "10", "15", "0", "15.5"]));
    }
    if rng.chance(6) {
        f.push_str(" # 註解");
    }
    f
}

/// 對合法公式做小幅破壞：製造「差一點就合法」的輸入。
fn mutate(rng: &mut Rng, s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return s.to_string();
    }
    const JUNK: &[char] = &['(', ')', '{', '}', ',', '+', '-', '*', '/', 'd', 'k', 'x', '.', ' ', '#', '中'];
    for _ in 0..1 + rng.below(3) {
        let i = rng.below(chars.len());
        match rng.below(4) {
            0 => {
                chars.remove(i);
            }
            1 => chars.insert(i, *rng.pick(JUNK)),
            2 => chars[i] = *rng.pick(JUNK),
            _ => {
                let c = chars[i];
                chars.insert(rng.below(chars.len() + 1), c);
            }
        }
        if chars.is_empty() {
            break;
        }
    }
    chars.into_iter().collect()
}

// ------------------------------------------------------------------ 檢查

/// 檢查一次成功的擲骰是否自洽。任何 panic 都會讓測試失敗。
fn check_result(formula: &str, rr: &RollResult) {
    let _ = format_result(rr);
    assert!(
        rr.total.value.is_finite() || rr.total.value.is_nan(),
        "{formula:?}: total 不該是 inf（{:?}）",
        rr.total.value
    );

    for g in &rr.groups {
        // 一組骰子最多 1000 顆（MAX_EXPLODE_DICE）——防爆炸失控。
        assert!(
            g.rolls.len() <= 1001,
            "{formula:?}: 骰子數量失控（{}）",
            g.rolls.len()
        );

        // 精確值域，靠 `g.kind`（不再靠 `faces == 0` 猜，也不靠 `notation`
        // 字串——`notation` 是原始碼切片，`1d1002r2r4df` 的 `df` 是
        // 「扣失敗點數」，用字串判斷骰型會誤判）。
        let domain = g.kind.domain(g.faces.max(0) as u32);
        let (lo, hi) = (domain.lo, domain.hi);
        for d in &g.rolls {
            assert!(
                (lo..=hi).contains(&d.value),
                "{formula:?}: {d:?} 超出 {:?} 的值域 [{lo},{hi}]（notation={:?}）",
                g.kind,
                g.notation
            );
            assert!(
                d.explode_depth <= 1001,
                "{formula:?}: 爆炸深度失控（{}）",
                d.explode_depth
            );
            // 重骰鏈尾端 == 目前值，**除非** min/max 之後又改過值：
            // 執行順序是 reroll → explode → min/max，min/max 只改 `value`
            // 與 `replaced_from`，不動 history。渲染器靠「尾端不等就補上」
            // 處理這種情況（見 `die_repr`）。
            if let Some(h) = &d.reroll_history {
                assert!(!h.is_empty(), "{formula:?}: 重骰鏈不該是空的");
                assert!(
                    h.last() == Some(&d.value) || d.replaced_from.is_some(),
                    "{formula:?}: 重骰鏈尾端 {:?} 與值 {} 不符，且沒有 replaced_from",
                    h.last(),
                    d.value
                );
            }
        }
    }
}

fn exercise(formula: &str) {
    if formula.len() > 512 {
        return;
    }
    match roll(formula, &mut SeededRng::new(0)) {
        Ok(rr) => check_result(formula, &rr),
        Err(e) => {
            let msg = e.message();
            assert!(
                msg.contains("錯誤"),
                "錯誤訊息必須含「錯誤」才能直接貼進 Discord：{formula:?} → {msg:?}"
            );
            assert!(!msg.contains("```"), "訊息本體不該含圍籬：{msg:?}");

            // 位置指示的結構不變量
            let pointer = e.pointer();
            if let Some(span) = e.span {
                let _ = span;
                let mut lines = pointer.lines();
                let (Some(body), Some(caret)) = (lines.next(), lines.next()) else {
                    panic!("pointer 必須是兩行：{formula:?} → {pointer:?}");
                };
                assert!(lines.next().is_none(), "pointer 只能兩行：{pointer:?}");
                // 插入符必須全在同一段，且對得上 span 寬度
                let width = span.end.saturating_sub(span.start).clamp(1, 100);
                assert_eq!(
                    caret.trim_start_matches(' ').chars().count(),
                    width,
                    "{formula:?}: 插入符寬度不符 → {caret:?}"
                );
                assert!(caret.trim_start().chars().all(|c| c == '^'));
                // 公式被裁切時前後要有省略號
                if body.chars().count() > 104 {
                    panic!("{formula:?}: 指示塊未裁切 → {} 字元", body.chars().count());
                }
            } else {
                assert!(pointer.is_empty(), "沒有 span 就不該有指示塊");
                // 唯一例外：Parse 的公式改走**行內** code span（公式可能含
                // `*` / `_`，裸放會被 Discord 吃字），其餘錯誤不變。
                // 但空公式（`,r` 只給空白）沒有東西可包，就該原樣。
                const PREFIX: &str = "錯誤：無法解析的公式：";
                let display = e.for_display();
                match &e.error {
                    RollError::Parse(_) if msg.len() > PREFIX.len() => {
                        assert!(
                            display.starts_with(&format!("{PREFIX}`")),
                            "{formula:?} → {display:?}"
                        );
                        assert!(
                            display.contains(msg.trim_start_matches(PREFIX)),
                            "{formula:?} → {display:?} 漏了公式"
                        );
                    }
                    _ => assert_eq!(display, msg, "{formula:?}"),
                }
            }
        }
    }
}

// -------------------------------------------------------------------- 測試

#[test]
fn grammar_aware_formulas_never_panic() {
    let mut rng = Rng(0x0BAD_F00D_1234_5678);
    for _ in 0..80_000 {
        exercise(&gen_formula(&mut rng));
    }
}

#[test]
fn mutated_formulas_never_panic() {
    let mut rng = Rng(0xFACE_B00C_5678_1234);
    for _ in 0..80_000 {
        let base = gen_formula(&mut rng);
        exercise(&mutate(&mut rng, &base));
    }
}

#[test]
fn random_bytes_never_panic() {
    const ALPHABET: &[u8] = b"dDfFcC0123456789+-*/(){}<>=,. \t#%";
    let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
    for _ in 0..60_000 {
        let len = 1 + rng.below(40);
        let s: String = (0..len)
            .map(|_| {
                if rng.chance(8) {
                    char::from_u32(0x80 + rng.below(0x700) as u32).unwrap_or('?')
                } else {
                    ALPHABET[rng.below(ALPHABET.len())] as char
                }
            })
            .collect();
        exercise(&s);
    }
}

#[test]
fn long_but_legal_bodies_never_panic() {
    let cases = [
        format!("{}1d6{}", "(".repeat(100), ")".repeat(100)),
        format!("{}1d6{}", "(".repeat(101), ")".repeat(101)),
        "1d6".repeat(170),
        format!("{{{}}}", vec!["1d6"; 60].join(",")),
        "1d6k1".repeat(100),
        "1d6x=6kh".repeat(60),
        "100d6rr>=1".repeat(50),
        format!("{}1", "1d6+".repeat(120)),
        "9999999999999999".repeat(30),
        format!("{{{},}}", vec!["df"; 40].join(",")),
    ];
    for f in &cases {
        exercise(f);
    }
}

#[test]
fn fuzzing_reaches_real_evaluation_paths() {
    // 防止產生器退化成「全部在 lexer 就失敗」而讓上面的測試變空包彈
    let mut rng = Rng(0x0BAD_F00D_1234_5678);
    let (mut ok, mut domain_err) = (0, 0);
    for _ in 0..20_000 {
        let f = gen_formula(&mut rng);
        if f.len() > 512 {
            continue;
        }
        match roll(&f, &mut SeededRng::new(0)) {
            Ok(_) => ok += 1,
            Err(e) => {
                if !e.to_string().contains("無法解析") {
                    domain_err += 1;
                }
            }
        }
    }
    assert!(ok > 3_000, "只成功擲骰 {ok} 次，產生器太弱");
    assert!(domain_err > 100, "只走到 {domain_err} 次領域驗證，覆蓋不足");
}
