//! 對外 API：`roll()` / `format_result()` / `split_formula_comment()`。
//!
//! 這層不碰 Python，可用純 Rust 測試；PyO3 綁定（`lib.rs`）只負責型別轉換。

use crate::error::{RollError, Span, SpannedRollError};
use crate::evaluator::{self, Evaluator, Value};
use crate::parser;
use crate::rng::{DiceRng, SeededRng};
use crate::types::*;

/// 解析 + 驗證 + 擲骰。
///
/// 錯誤一律附上**原始碼位置**（若指得出來）；`engine` 是唯一知道完整公式的
/// 地方，所以在這裡把 `formula` 補進錯誤，讓指示箭頭畫得出來。
pub fn roll(formula: &str, rng: &mut dyn DiceRng) -> Result<RollResult, SpannedRollError> {
    let src = parser::strip_formula(formula)?;
    let parsed = parser::parse(src).map_err(|e| e.with_formula(src))?;

    let mut ev = Evaluator::new(rng);
    // 執行期錯誤（除以零、動態骰超界）目前指不出單一位置
    let (value, display) = ev
        .eval(&parsed.expr)
        .map_err(|e| SpannedRollError::new(e).with_formula(src))?;
    let groups = ev.into_groups();

    let (total, parts) = match value {
        Value::Scalar(v) => (v, None),
        Value::Multi(parts) => {
            // Python `sum(values)`：累加器從 int 0 開始
            let total = py_sum(parts.iter().map(|p| p.value));
            let rendered: Vec<String> = parts
                .iter()
                .map(|p| {
                    if evaluator::self_contained(&p.render, p.value.value) {
                        p.render.clone()
                    } else {
                        format!("{} = {}", p.render, evaluator::fmt_num(p.value.value))
                    }
                })
                .collect();
            (total, Some(rendered))
        }
    };

    let check = parsed.check.map(|c| CheckResult {
        op: c.op.as_str().to_string(),
        target: c.target,
        passed: c.op.eval(total.value, c.target.value),
    });

    // 純數學（沒有骰子也沒有檢查）時，顯示原輸入字串
    let base = if !groups.is_empty() || check.is_some() {
        display
    } else {
        src.to_string()
    };
    let terms = match &check {
        Some(c) => format!(
            "{base} {} {} → {}",
            c.op,
            evaluator::fmt_num(c.target.value),
            if c.passed { "成功" } else { "失敗" }
        ),
        None => base,
    };

    Ok(RollResult {
        formula: src.to_string(),
        total,
        terms,
        check,
        groups,
        parts,
    })
}

/// 渲染成 Discord 訊息（行內 code span 不巢狀）。
///
/// 形狀：
/// - 無 check、單組：``14`(6+6+1+1)` ``
/// - 無 check、多組：``14`(6+6+1+1)`+3+2`(2)` = 19``
/// - 無 check、裸骰池：每個 group 一行
/// - 有 check：``11`(11)`+2 = 13 >= 15 → 失敗``
pub fn format_result(rr: &RollResult) -> String {
    let Some(check) = &rr.check else {
        let terms = rr.terms.trim_end();
        if let Some(parts) = &rr.parts {
            return parts.join("\n");
        }
        if evaluator::self_contained(terms, rr.total.value) {
            return terms.to_string();
        }
        return format!("{terms} = {}", evaluator::fmt_num(rr.total.value));
    };

    let suffix = format!(
        " {} {} → {}",
        check.op,
        evaluator::fmt_num(check.target.value),
        if check.passed { "成功" } else { "失敗" }
    );
    let base = rr.terms.strip_suffix(&suffix).unwrap_or(&rr.terms).trim_end();
    if evaluator::self_contained(base, rr.total.value) {
        format!("{base}{suffix}")
    } else {
        format!("{base} = {}{suffix}", evaluator::fmt_num(rr.total.value))
    }
}

/// 把 `src` 切成 (公式, 註解)：公式 = 最長「可成功擲骰」的前綴。
///
/// 讓 `,r {4d6, 3d8}kh 註解` 與 `,r 2d6 註解` 都能運作——公式本身可含空白
/// （池的大括號內），因此用「最長可解析前綴」而非單純切空白。
///
/// 探測用獨立的 [`SeededRng`]，**不消耗**呼叫端注入的 RNG（與舊 Python 用
/// 模組層 `_DEFAULT_RNG` 探測的行為一致：不干擾注入的序列）。
pub fn split_formula_comment(src: &str) -> Result<(String, String), SpannedRollError> {
    let src_stripped = src.trim();
    if src_stripped.is_empty() {
        return Err(SpannedRollError::new(RollError::Parse(src.to_string())).with_formula(src));
    }

    let probe_ok = |candidate: &str| roll(candidate, &mut SeededRng::new(0)).is_ok();

    if probe_ok(src_stripped) {
        return Ok((src_stripped.to_string(), String::new()));
    }

    let mut i = src_stripped.len();
    while i > 0 {
        let Some(space_pos) = src_stripped[..i].rfind(' ') else {
            break;
        };
        let prefix = src_stripped[..space_pos].trim_end();
        if prefix.is_empty() {
            break;
        }
        if probe_ok(prefix) {
            let comment = src_stripped[space_pos + 1..].trim_start();
            return Ok((prefix.to_string(), comment.to_string()));
        }
        i = space_pos;
    }

    Err(no_prefix_parses(src, src_stripped))
}

/// 整體解析失敗時的錯誤：盡可能指出**第一個詞**裡的失敗點。
///
/// `split_formula_comment` 的前綴是由後往前砍的，第一個詞是最後一個候選：
/// 連它都不行，問題就一定落在第一個詞裡（否則早就切出註解、成功返回了）。
/// 所以拿它的解析失敗點當位置——使用者打錯一個 token（`,r 1d4+100dx<=6`）
/// 是這條路徑最常見的情況，只回顯公式不指出位置等於沒說。
///
/// 第一種失敗指不出位置：語法合法但擲不出來（`1d0`、`1/0`、`101d6`）。
/// 那種情況留在訊息本體，語意與舊版相同。
fn no_prefix_parses(src: &str, src_stripped: &str) -> SpannedRollError {
    let error = RollError::Parse(src.to_string());
    // 第一個詞在 `src` 裡的起點（前導空白不參與解析，位移要補回來）
    let lead = src.len() - src.trim_start().len();
    let word = src_stripped.split_whitespace().next().unwrap_or(src_stripped);
    let syntax_failure = parser::parse(word)
        .err()
        .filter(|e| matches!(e.error, RollError::Parse(_)))
        .and_then(|e| e.span);
    match syntax_failure {
        Some(span) => SpannedRollError::at(
            error,
            Span::new(lead + span.start, lead + span.end),
        )
        .with_formula(src),
        None => SpannedRollError::new(error).with_formula(src),
    }
}
