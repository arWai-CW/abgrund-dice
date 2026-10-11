//! 引擎層測試（純 Rust，不經 Python）。
//!
//! Python 端 `tests/test_rolling_differential.py` 已經拿凍結的 Python oracle
//! 對拍過整個語法空間，是正確性的主要證據。這裡補的是 **Python 測不到** 的面相：
//! CSPRNG 路徑、`SeededRng`、內部不變量、以及 stack-overflow / 長度上限這類
//! 只有在 Rust 才會失控的東西。

use abgrund_dice::engine::{format_result, roll, split_formula_comment};
use abgrund_dice::error::{RollError, Span, SpannedRollError};
use abgrund_dice::rng::{CryptoRng, SeededRng};
use abgrund_dice::evaluator::self_contained;
use abgrund_dice::types::{DieKind, MAX_DICE_DETAIL};

fn roll_seeded(formula: &str, seed: u64) -> abgrund_dice::types::RollResult {
    roll(formula, &mut SeededRng::new(seed)).expect("formula should roll")
}

fn roll_err(formula: &str) -> SpannedRollError {
    roll(formula, &mut SeededRng::new(0))
        .map(|_| panic!("expected error for {formula:?}"))
        .unwrap_err()
}

fn err(formula: &str) -> String {
    roll_err(formula).message()
}

// --------------------------------------------------------------- 正式 RNG 路徑

#[test]
fn csprng_path_keeps_dice_in_range() {
    let mut rng = CryptoRng::new();
    for formula in ["4d6", "3d10", "2d8k", "10d20", "4df", "8dc"] {
        for _ in 0..40 {
            let rr = roll(formula, &mut rng).unwrap();
            for g in &rr.groups {
                let (lo, hi) = if g.notation.contains("df") {
                    (-1, 1)
                } else if g.notation.contains("dc") {
                    (0, 1)
                } else {
                    (1, g.faces)
                };
                for d in &g.rolls {
                    assert!(
                        (lo..=hi).contains(&d.value),
                        "{formula}: {d:?} outside [{lo},{hi}]"
                    );
                }
            }
        }
    }
}

#[test]
fn csprng_produces_distinct_rolls() {
    // 同一公式連續擲 200 次，至少要有一次不同——否則 RNG 根本沒在動
    let mut rng = CryptoRng::new();
    let totals: Vec<i64> = (0..200)
        .map(|_| roll("10d20", &mut rng).unwrap().groups[0].total)
        .collect();
    assert!(
        totals.iter().any(|t| *t != totals[0]),
        "all 200 rolls identical: {}",
        totals[0]
    );
}

#[test]
fn seeded_rng_is_reproducible_across_calls() {
    for formula in ["4d6k3", "6d10x", "2d8r<3", "{4d6,3d8}kh", "(2d4)d8"] {
        assert_eq!(
            roll_seeded(formula, 42).terms,
            roll_seeded(formula, 42).terms,
            "not reproducible: {formula}"
        );
    }
}

// ------------------------------------------------------------------- 不變量

#[test]
fn group_total_is_sum_of_kept_for_plain_dice() {
    for seed in 0..24 {
        for formula in ["4d6", "6d10", "20d20", "2d4"] {
            let rr = roll_seeded(formula, seed);
            for g in &rr.groups {
                let kept: i64 = g.rolls.iter().filter(|d| !d.dropped).map(|d| d.value).sum();
                assert_eq!(g.total, kept, "{formula} seed {seed}");
            }
        }
    }
}

#[test]
fn keep_drop_marks_exactly_n_dice() {
    for seed in 0..24 {
        for (formula, expect_dropped) in
            [("4d6k3", 1), ("4d6kh2", 2), ("4d6kl", 3), ("4d6dh2", 2), ("4d6d", 1)]
        {
            let rr = roll_seeded(formula, seed);
            let g = &rr.groups[0];
            let dropped = g.rolls.iter().filter(|d| d.dropped).count();
            assert_eq!(dropped, expect_dropped, "{formula} seed {seed}");
        }
    }
}

#[test]
fn explode_children_always_carry_flag_and_depth() {
    for seed in 0..40 {
        let rr = roll_seeded("6d10x", seed);
        let g = &rr.groups[0];
        for (i, d) in g.rolls.iter().enumerate() {
            assert_eq!(d.exploded, i >= 6, "seed {seed} index {i}");
            assert_eq!(d.explode_depth > 0, i >= 6);
        }
    }
}

#[test]
fn reroll_finals_never_match_recursive_cp() {
    for seed in 0..40 {
        let g = roll_seeded("4d6rr<3", seed).groups[0].clone();
        for d in &g.rolls {
            assert!(d.value >= 3, "seed {seed}: rr<3 left {}", d.value);
        }
    }
}

#[test]
fn notation_is_the_verbatim_source_slice() {
    for (formula, expected) in [
        ("4d6k3", "4d6k3"),
        ("2d8r2r4", "2d8r2r4"),
        ("3d10k", "3d10k"),
        ("4df", "4df"),
        ("4dc", "4dc"),
    ] {
        let rr = roll_seeded(formula, 0);
        assert_eq!(rr.groups[0].notation, expected, "{formula}");
    }
}

#[test]
fn dynamic_dice_groups_keep_their_own_notation() {
    // `(2d4)d8` → 內層先擲，所以 groups[0] 是內層
    let rr = roll_seeded("(2d4)d8", 0);
    assert_eq!(rr.groups.len(), 2);
    assert_eq!(rr.groups[0].notation, "2d4");
    assert_eq!(rr.groups[1].notation, "(2d4)d8");
}

#[test]
fn nested_group_order_is_inner_before_outer() {
    // RNG 消耗順序必須與舊 Python 兩趟走訪一致
    let rr = roll_seeded("1d(1d20)", 0);
    assert_eq!(rr.groups.len(), 2);
    let faces = rr.groups[1].faces;
    assert_eq!(rr.groups[0].rolls[0].value, faces);
    assert_eq!(rr.groups[1].rolls[0].value, rr.total.value as i64);
}

#[test]
fn pool_suffix_is_the_verbatim_source_tail() {
    let rr = roll_seeded("{6d6,5d8,4d10,3d12}cs>15", 0);
    assert!(rr.terms.ends_with("}cs>15"), "{}", rr.terms);
    assert_eq!(rr.groups.len(), 4);
}

#[test]
fn bare_multi_pool_never_sums_silently() {
    let rr = roll_seeded("{4d6,3d8}", 7);
    let parts = rr.parts.as_ref().expect("bare pool → parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(rr.total.value, rr.groups.iter().map(|g| g.total).sum::<i64>() as f64);
    assert!(!format_result(&rr).contains(" = "));
}

// ------------------------------------------------------- 上限 / 不可 panic

#[test]
fn deep_nesting_is_an_error_not_a_stack_overflow() {
    let formula = format!("{}1{}", "(".repeat(200), ")".repeat(200));
    assert!(formula.len() <= 512);
    let e = err(&formula);
    assert!(e.contains("錯誤"), "{e}");
}

#[test]
fn deeply_nested_that_stays_under_cap_still_works() {
    let formula = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    assert_eq!(roll_seeded(&formula, 0).total.value, 1.0);
}

#[test]
fn formula_length_limit_is_checked_before_parsing() {
    let formula = "1".repeat(513);
    assert!(err(&formula).contains("公式過長"));
}

#[test]
fn explode_cap_prevents_runaway() {
    // 100d1x would explode forever: cap the count and check it terminates
    let rr = roll_seeded("100d1x1=1", 0);
    let g = &rr.groups[0];
    assert!(g.rolls.len() <= 1000, "{}", g.rolls.len());
}

#[test]
fn always_true_reroll_does_not_spin() {
    // 舊版 regression：`100d1rr` 會空轉 100×1000 次
    let start = std::time::Instant::now();
    let rr = roll_seeded("100d1rr", 0);
    assert!(start.elapsed().as_millis() < 500);
    assert!(rr.groups[0].rolls.iter().all(|d| d.value == 1 && !d.rerolled));
}

// ------------------------------------------------------------------ 錯誤訊息

#[test]
fn error_messages_match_the_python_oracle_verbatim() {
    for (formula, expected) in [
        ("101d6", "錯誤：骰數必須介於 1~100"),
        ("0d6", "錯誤：骰數必須介於 1~100"),
        ("1d0", "錯誤：骰面必須介於 1~100000"),
        ("1d-4", "錯誤：骰面必須介於 1~100000"),
        ("1d100001", "錯誤：骰面必須介於 1~100000"),
        ("1/0", "錯誤：除以零"),
        ("4dcx", "錯誤：硬幣不支援該修飾詞"),
        ("4dc kh", "錯誤：硬幣不支援該修飾詞"),
        ("4d6x2000", "錯誤：爆炸次數上限為 1000"),
        ("4d6min100001", "錯誤：數值必須介於 0~100000"),
        ("4d6ms100001", "錯誤：數值必須介於 0~100000"),
    ] {
        assert_eq!(err(formula), expected, "{formula:?}");
    }
}

#[test]
fn parse_failures_all_mention_無法解析() {
    for formula in [
        "abc", "1d20>=", "1d20>=15x", "1d20x>=", "4dfcs", "4fdx", "{}", "{4d6}khx", "4d",
    ] {
        let e = err(formula);
        assert!(e.contains("錯誤"), "{formula:?}: {e}");
        assert!(e.contains("無法解析"), "{formula:?}: {e}");
    }
}

// --------------------------------------------------------------------- 渲染

#[test]
fn single_group_render_is_self_contained() {
    let rr = roll_seeded("1d1", 0);
    assert_eq!(rr.terms, "1`(1)`");
    assert_eq!(format_result(&rr), "1`(1)`");
}

#[test]
fn check_render_appends_total_once() {
    let rr = roll_seeded("1d1+2>=15", 0);
    assert_eq!(rr.terms, "1`(1)`+2 >= 15 → 失敗");
    assert_eq!(format_result(&rr), "1`(1)`+2 = 3 >= 15 → 失敗");
}

#[test]
fn pure_math_renders_the_raw_source() {
    let rr = roll_seeded("1 + 2  *3", 0);
    assert_eq!(rr.total.value, 7.0);
    assert!(rr.groups.is_empty());
    assert_eq!(rr.terms, "1 + 2  *3");
}

// ------------------------------------------------------- int / float 身分

#[test]
fn division_always_yields_float_even_when_integral() {
    let rr = roll_seeded("100/4", 0);
    assert!(rr.total.is_float, "100/4 must be a Python float");
    assert_eq!(rr.total.value, 25.0);
}

#[test]
fn floor_ceil_round_always_yield_int() {
    for formula in ["floor(10/4)", "ceil(10/4)", "round(10/4)", "7/2", "7+1"] {
        let rr = roll_seeded(formula, 0);
        let want_float = formula == "7/2";
        assert_eq!(rr.total.is_float, want_float, "{formula}");
    }
}

#[test]
fn dice_totals_are_always_int() {
    for formula in ["4d6", "4d6cs>=4", "4dc", "4df", "3d6ms1"] {
        assert!(!roll_seeded(formula, 0).total.is_float, "{formula}");
    }
}

// ------------------------------------------------------- split_formula_comment

#[test]
fn split_formula_comment_handles_pools_with_spaces() {
    assert_eq!(
        split_formula_comment("{4d6, 3d8}kh 註解").unwrap(),
        ("{4d6, 3d8}kh".to_string(), "註解".to_string())
    );
    assert_eq!(
        split_formula_comment("2d6 + 3 註解").unwrap(),
        ("2d6 + 3".to_string(), "註解".to_string())
    );
    assert_eq!(
        split_formula_comment("2d6").unwrap(),
        ("2d6".to_string(), String::new())
    );
}

#[test]
fn split_formula_comment_rejects_unparseable_input() {
    assert!(matches!(
        split_formula_comment("完全沒有公式").unwrap_err().error,
        RollError::Parse(_)
    ));
    assert!(matches!(
        split_formula_comment("   ").unwrap_err().error,
        RollError::Parse(_)
    ));
}

#[test]
fn split_failure_still_points_into_the_first_word() {
    // `,r <一個打錯的 token>` 是這條路徑最常見的情況：只回顯公式等於沒說，
    // 位置從第一個詞的解析失敗點拿（前綴是由後往前砍的，第一個詞是最後候選）
    let e = split_formula_comment("1d4+100dx<=6+1d20>3000").unwrap_err();
    assert_eq!(e.span, Some(Span::new(4, 12)));
    assert_eq!(
        e.for_display(),
        "錯誤：無法解析的公式：`1d4+`***100dx<=6***`+1d20>3000`"
    );

    // 語法合法但擲不出來（1d0、1/0）：指不出位置，訊息與舊版相同
    let e = split_formula_comment("1d0").unwrap_err();
    assert_eq!(e.span, None);
    assert_eq!(e.message(), "錯誤：無法解析的公式：1d0");
    assert_eq!(e.for_display(), "錯誤：無法解析的公式：`1d0`");
}

// ---------------------------------------------------------------- 效能 sanity

#[test]
fn parsing_is_fast_enough_to_not_matter() {
    // 不是 benchmark，只是防止意外退化成指數行為
    let start = std::time::Instant::now();
    for _ in 0..2_000 {
        roll("4d6k3+2d8kh1+1d20>=15", &mut SeededRng::new(1)).unwrap();
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 3_000, "2000 rolls took {elapsed:?}");
}


// ------------------------------------------------- min / max 值域檢查

#[test]
fn minmax_out_of_range_is_rejected() {
    // 舊版會靜默擲出界外值：d1 出現 2、d6 出現 0
    for (formula, expected) in [
        ("1d1min2", "錯誤：min 不可大於 1"),
        ("100d1min2max8", "錯誤：min 不可大於 1"),
        ("1d20r2min10001", "錯誤：min 不可大於 20"),
        ("2d2rr>=6min3", "錯誤：min 不可大於 2"),
        ("4d6max0", "錯誤：max 不可小於 1"),
        ("4dc min2", "錯誤：min 不可大於 1"),
        ("(1d20)d8min9", "錯誤：min 不可大於 8"),
        // 全域合理性檢查先擋（順序與 oracle 一致）
        ("4d6min100001", "錯誤：數值必須介於 0~100000"),
    ] {
        assert_eq!(err(formula), expected, "{formula:?}");
    }
}

#[test]
fn minmax_in_range_is_allowed_including_harmless_noops() {
    // 無害 no-op 必須放行，否則長公式會被誤擋
    for formula in [
        "4d6min2",
        "4d6max4",
        "4d6min2max4",
        "4d6min0",
        "4d6min1",
        "4d6max6",
        "4d6max8",
        "4d6max100",
        "1d1min1",
        "4dc max0",
        "4dc min0",
        "4d6rr<3min2",
        "4d6r<3min2max8",
    ] {
        roll(formula, &mut SeededRng::new(0)).unwrap_or_else(|e| {
            panic!("{formula:?} 不該被擋：{e}");
        });
    }
}

#[test]
fn dice_values_always_stay_inside_their_domain() {
    // 新規則保證的不變量：骰值永不超出版面值域
    let mut rng = SeededRng::new(3);
    for faces in [1, 2, 4, 6, 20, 100, 100_000] {
        for count in [1, 4, 100] {
            for suffix in ["", "min1", "min2", "max0", "max1", "max8", "min2max8"] {
                let formula = format!("{count}d{faces}{suffix}");
                let Ok(rr) = roll(&formula, &mut rng) else {
                    continue; // 被新規則擋掉是合理的
                };
                for g in &rr.groups {
                    let d = g.kind.domain(g.faces as u32);
                    for die in &g.rolls {
                        assert!(
                            (d.lo..=d.hi).contains(&die.value),
                            "{formula}: {} 超出 {:?} 值域 [{},{}]",
                            die.value,
                            g.kind,
                            d.lo,
                            d.hi
                        );
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------- DiceGroup.kind

#[test]
fn groups_are_self_describing() {
    for (formula, idx, kind) in [
        ("4d6", 0, DieKind::Normal),
        ("4df", 0, DieKind::Fate),
        ("4dF", 0, DieKind::Fate),
        ("4dc", 0, DieKind::Coin),
        ("3dcc1", 0, DieKind::Coin),
        ("(2d4)df", 1, DieKind::Fate),
        ("1d(1d20)", 0, DieKind::Normal),
    ] {
        let rr = roll_seeded(formula, 0);
        assert_eq!(rr.groups[idx].kind, kind, "{formula} group {idx}");
        assert_eq!(rr.groups[idx].kind.as_str(), kind.as_str());
    }
}

#[test]
fn coin_and_fate_are_no_longer_ambiguous() {
    // 舊版的 faces == 0 對兩者都成立，無法區分
    let coin = roll_seeded("4dc", 0).groups[0].clone();
    let fate = roll_seeded("4df", 0).groups[0].clone();
    assert_eq!(coin.faces, fate.faces, "兩者的 faces 都是 0（正是問題所在）");
    assert_ne!(coin.kind, fate.kind, "kind 才能區分");
    assert_eq!(coin.kind, DieKind::Coin);
    assert_eq!(fate.kind, DieKind::Fate);
}

#[test]
fn kind_domains_are_distinct() {
    assert_eq!(DieKind::Normal.domain(6).lo, 1);
    assert_eq!(DieKind::Normal.domain(6).hi, 6);
    assert_eq!(DieKind::Fate.domain(0).lo, -1);
    assert_eq!(DieKind::Fate.domain(0).hi, 1);
    assert_eq!(DieKind::Coin.domain(0).lo, 0);
    assert_eq!(DieKind::Coin.domain(0).hi, 1);
}


// ------------------------------------------------------------ 錯誤位置指示

#[test]
fn errors_point_at_the_offending_span() {
    for (formula, expected_text, why) in [
        ("{4d6, 3d8}kh+1d0", Some("0"), "骰面 0"),
        ("100d1min2max8", Some("2"), "min 的參數"),
        ("4d6max0", Some("0"), "max 的參數"),
        ("4d6x2000", Some("2000"), "爆炸上限"),
        ("4dcx", Some("dcx"), "整個 ident"),
        ("abc", Some("abc"), "未知詞"),
        ("4d6@@", Some("@"), "第一個非法字元"),
        ("4d6k3+2d8kh1+1d20>=15x", Some("x"), "多出來的尾巴"),
        // 數字／括弧後面接了不是骰子產生式的 ident：整個骰子項都是問題
        ("100dx<=6 + 1", Some("100dx<=6"), "拚錯的骰子項（含條件）"),
        (
            "1d4+100dx<=6+1d20>3000",
            Some("100dx<=6"),
            "沒有空白也停在條件結束",
        ),
        ("4dfcs", Some("4dfcs"), "fate 不吃修飾詞"),
        ("4fdx", Some("4fdx"), "f/d 寫反"),
        ("4kh3", Some("4kh"), "修飾詞沒有骰子可修"),
        ("2*3dz", Some("3dz"), "拚錯的骰子項在乘法右側"),
        ("(2)dz", Some("(2)dz"), "動態骰數拚錯"),
        ("1d4*", Some("*"), "乘法後面沒有右運算元"),
        // 對照組：合法公式照舊擲得出來（更廣的清單見 botched_dice_does_not_swallow_valid_dice）
        ("(2)d6", None, "動態骰數"),
    ] {
        match expected_text {
            Some(expected) => {
                let e = roll_err(formula);
                let span = e
                    .span
                    .unwrap_or_else(|| panic!("{formula:?}: {why} 應該有位置"));
                assert_eq!(
                    &formula[span.start..span.end],
                    expected,
                    "{formula:?}: {why}"
                );
            }
            None => {
                roll(formula, &mut SeededRng::new(0)).unwrap_or_else(|e| {
                    panic!(
                        "{formula:?}: {why} 應該擲得出來，卻回報 {msg}",
                        msg = e.for_display()
                    )
                });
            }
        }
    }
}

#[test]
fn parse_errors_mark_the_offending_segment_inline() {
    // 形狀：前後段各包一個行內 code span，出錯的部分包 ***（粗斜體）
    for (formula, expected) in [
        (
            "1d4 + 100dx<=6 + 1d20 > 3000",
            "錯誤：無法解析的公式：`1d4 + `***100dx<=6***` + 1d20 > 3000`",
        ),
        (
            "1d4+100dx<=6+1d20>3000",
            "錯誤：無法解析的公式：`1d4+`***100dx<=6***`+1d20>3000`",
        ),
        // 公式本身有 `*`：前後段的 `*` 被 code span 保護，不會變成粗斜體
        ("2*3dz", "錯誤：無法解析的公式：`2*`***3dz***"),
        ("abc", "錯誤：無法解析的公式：***abc***"),
    ] {
        assert_eq!(roll_err(formula).for_display(), expected, "{formula:?}");
    }
}

#[test]
fn parse_errors_do_not_repeat_the_caret_block() {
    // 出錯部分已經行內標出來 → 不附圍籬箭頭（同一件事不說兩次）
    let e = roll_err("abc");
    assert_eq!(e.pointer(), "abc\n^^^");
    assert!(!e.for_display().contains("```"), "{}", e.for_display());
    // 語意錯誤維持原狀：訊息 + 圍籬內的指示塊
    assert!(roll_err("1d0").for_display().contains("```"));
}

#[test]
fn botched_dice_does_not_swallow_valid_dice() {
    // 數字後面接合法骰子 ident / 修飾詞的公式照舊擲得出來
    for formula in [
        "4d6", "4df", "4dc", "4 d6", "2*3d6", "1d20*3>=15", "round(2*3)",
        "{4d6, 3d8}kh+1d20", "1d(2d6)+3",
    ] {
        roll(formula, &mut SeededRng::new(0)).unwrap_or_else(|e| {
            panic!("{formula:?} 應該擲得出來，卻回報 {msg}", msg = e.for_display())
        });
    }
}

#[test]
fn pointer_aligns_the_caret_under_the_span() {
    let e = roll_err("{4d6, 3d8}kh+1d0");
    let pointer = e.pointer();
    let mut lines = pointer.lines();
    let body = lines.next().unwrap();
    let caret = lines.next().unwrap();
    assert_eq!(body, "{4d6, 3d8}kh+1d0");
    assert_eq!(caret, "               ^");
    assert_eq!(lines.next(), None, "pointer 只能是兩行");
}

#[test]
fn for_display_fences_the_pointer_but_message_does_not() {
    let e = roll_err("1d0");
    assert_eq!(e.message(), "錯誤：骰面必須介於 1~100000");
    assert!(!e.message().contains("```"));
    let display = e.for_display();
    assert!(display.starts_with(&e.message()));
    assert!(display.contains("```"), "公式可能含 * / _，必須用圍籬");
    assert!(display.contains("1d0"));
}

#[test]
fn execution_errors_report_no_span_but_keep_the_message() {
    let e = roll_err("1/0");
    assert_eq!(e.message(), "錯誤：除以零");
    assert_eq!(e.span, None);
    assert_eq!(e.pointer(), "");
    assert_eq!(e.for_display(), "錯誤：除以零", "沒有位置就不加圍籬");
}

#[test]
fn pointer_window_keeps_long_formulas_small() {
    let formula = format!("{}1d0", "1d6+".repeat(100));
    assert!(formula.len() <= 512);
    let e = roll_err(&formula);
    let pointer = e.pointer();
    let mut lines = pointer.lines();
    let body = lines.next().unwrap();
    let caret = lines.next().unwrap();
    assert!(body.chars().count() <= 104, "視窗太寬：{}", body.chars().count());
    assert!(body.starts_with('…') || body.ends_with('…'));
    // 插入符仍對準錯誤字元
    let col = caret.find('^').unwrap();
    assert_eq!(body.chars().nth(col).unwrap(), '0');
}

#[test]
fn span_is_relative_to_the_stripped_formula() {
    let raw = "1d0 # 很長的註解";
    let e = roll_err(raw);
    let span = e.span.unwrap();
    // 錯誤指示只針對剝離註解後的公式
    assert_eq!(&"1d0"[span.start..span.end], "0");
    assert!(!e.pointer().contains("註解"));
}

#[test]
fn formula_too_long_points_at_the_cutoff() {
    let formula = "1".repeat(513);
    let e = roll_err(&formula);
    assert_eq!(e.message(), "錯誤：公式過長");
    assert_eq!(e.span, Some(Span::new(512, 513)));
}


// -------------------------------------------------------- 明細隱藏（門檻）

#[test]
fn detail_hiding_threshold_is_exclusive_on_the_declared_count() {
    // 20 顆照列明細；21 顆只給「總計 + 骰組規格」
    let shown = roll_seeded("20d6", 0);
    assert_eq!(shown.groups[0].rolls.len(), 20);
    assert_eq!(shown.terms.matches('+').count(), 19, "{}", shown.terms);

    let hidden = roll_seeded("21d6", 0);
    assert_eq!(hidden.groups[0].rolls.len(), 21);
    assert_eq!(hidden.terms, format!("{}`(21d6)`", hidden.total.value as i64));
    assert!(!hidden.terms.contains('+'), "{}", hidden.terms);
}

#[test]
fn detail_hiding_uses_actual_dice_not_the_declared_count() {
    // `4d6x<=6` 宣告 4 顆，實際爆炸出 1000 顆——用宣告值判斷會擋不住
    let rr = roll_seeded("4d6x<=6", 0);
    assert_eq!(rr.groups[0].rolls.len(), 1000);
    assert_eq!(rr.terms, format!("{}`(4d6x<=6)`", rr.total.value as i64));

    // 相反的陷阱：`4d6k1` 擲 4 顆但留下 1 顆，仍應列明細
    let kept = roll_seeded("4d6k1", 0);
    assert_eq!(kept.groups[0].rolls.len(), 4);
    assert_eq!(kept.terms.matches('+').count(), 3, "{}", kept.terms);
}

#[test]
fn hidden_detail_renders_are_self_contained() {
    for formula in ["21d6", "100d6", "100d6x<=6", "21df", "21dc"] {
        let rr = roll_seeded(formula, 0);
        assert!(
            self_contained(&rr.terms, rr.total.value),
            "{formula}: {:?} 應該要是自帶總計的形狀",
            rr.terms
        );
    }
}

#[test]
fn hiding_changes_only_the_render_not_the_numbers() {
    for formula in ["100d6", "100d6k3", "4d6x<=6", "100d6cs>=4", "100d6min2"] {
        let got = roll_seeded(formula, 3);
        let g = &got.groups[0];
        assert!(g.rolls.len() > MAX_DICE_DETAIL, "{formula}");
        // 每一顆骰的欄位都還在，值域也仍然成立
        let domain = g.kind.domain(g.faces as u32);
        for d in &g.rolls {
            assert!((domain.lo..=domain.hi).contains(&d.value), "{formula}");
        }
        assert!(got.terms.len() < 40, "{formula}: 長度沒壓下來");
    }
}

#[test]
fn small_rolls_keep_every_die_visible() {
    // 門檻以下的行為完全不能變（回歸保護）
    for (formula, dice) in [("4d6", 4), ("4d6k3", 4), ("4d6d2", 4), ("20d6", 20), ("4df", 4)] {
        let rr = roll_seeded(formula, 0);
        assert_eq!(rr.groups[0].rolls.len(), dice);
        assert_eq!(
            rr.terms.matches('+').count(),
            dice - 1,
            "{formula} 明細被誤藏：{}",
            rr.terms
        );
    }
}
