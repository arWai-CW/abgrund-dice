//! 錯誤型別與位置指示。
//!
//! 訊息字串與舊 `modules/RollingModule.py` 逐字相同（一律含「錯誤」），
//! 因為 `cogs/diceRoll.py` 直接把例外訊息貼進 Discord 訊息。
//!
//! 在此之上加了**位置指示**：使用者抱怨「roll 沒有檢查／自動補全，組合很多
//! 又很長，很容易打錯」，所以錯誤要能指出**哪個字元**出問題。
//! 兩種顯示形狀（見 [`SpannedRollError::for_display`]）：
//!
//! - 無法解析的公式 → 行內：出錯部分包 `***`（粗斜體），其餘包 code span
//! - 其他錯誤 → 訊息 + 圍籬內對齊的箭頭
//!
//! 訊息的「本體」（[`SpannedRollError::message`]）永遠不含這些裝飾，
//! 與舊版逐字相同——差分測試對拍的就是它。

use once_cell::sync::Lazy;
use pyo3::prelude::*;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Mutex;

/// Python 例外類別（`rolling_rs.RollError`）。
///
/// `create_exception!` 以識別字當 Python 名稱，因此型別名必須是 `RollError`；
/// 放在子模組避免與下面的 Rust `RollError` 衝突。
mod exc {
    pyo3::create_exception!(
        rolling_rs,
        RollError,
        pyo3::exceptions::PyException,
        "骰子公式錯誤（解析／驗證／執行）。訊息一律含「錯誤」。"
    );
}

pub use exc::RollError as PyRollError;

/// 支援的語言
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    ZhTW, // 繁體中文（預設）
    En,   // 英文
}

/// 全域語言設定（執行期可變更）
static CURRENT_LANG: Lazy<Mutex<Language>> = Lazy::new(|| Mutex::new(Language::ZhTW));

/// 設定當前語言（可由 Python 呼叫）
pub fn set_language(lang: Language) {
    *CURRENT_LANG.lock().unwrap() = lang;
}

/// 取得當前語言
pub fn current_language() -> Language {
    *CURRENT_LANG.lock().unwrap()
}

/// 錯誤訊息的翻譯表：錯誤變種名稱 → 語言 → 訊息樣板。
/// 樣板使用 `{0}` 作為單一參數佔位符（與原本的 `Display` 實作相容）。
static TRANSLATIONS: Lazy<HashMap<&'static str, HashMap<Language, &'static str>>> =
    Lazy::new(|| {
        let mut m = HashMap::new();
        macro_rules! add {
            ($variant:ident, $zh:expr, $en:expr) => {
                let mut inner = HashMap::new();
                inner.insert(Language::ZhTW, $zh);
                inner.insert(Language::En, $en);
                m.insert(stringify!($variant), inner);
            };
        }

        add!(
            Parse,
            "錯誤：無法解析的公式：{0}",
            "Error: Unable to parse formula: {0}"
        );
        add!(
            DiceCountOutOfRange,
            "錯誤：骰數必須介於 1~100",
            "Error: Dice count must be between 1 and 100"
        );
        add!(
            FacesOutOfRange,
            "錯誤：骰面必須介於 1~100000",
            "Error: Face count must be between 1 and 100000"
        );
        add!(FormulaTooLong, "錯誤：公式過長", "Error: Formula too long");
        add!(DivisionByZero, "錯誤：除以零", "Error: Division by zero");
        add!(
            ExplodeCapExceeded,
            "錯誤：爆炸次數上限為 {0}",
            "Error: Explode cap exceeded {0}"
        );
        add!(
            MinTooLarge,
            "錯誤：min 不可大於 {0}",
            "Error: min cannot be greater than {0}"
        );
        add!(
            MaxTooSmall,
            "錯誤：max 不可小於 {0}",
            "Error: max cannot be smaller than {0}"
        );
        add!(
            ValueOutOfRange,
            "錯誤：數值必須介於 0~100000",
            "Error: Value must be between 0 and 100000"
        );
        add!(
            PoolLengthMismatch,
            "錯誤：骰池長度不同",
            "Error: Dice pool length mismatch"
        );
        add!(
            CoinModifierNotSupported,
            "錯誤：硬幣不支援該修飾詞",
            "Error: Modifier not supported for coin"
        );
        m
    });

/// 指示塊最寬幾個**字元**。超過就只顯示錯誤附近的視窗。
const POINTER_MAX_WIDTH: usize = 100;

/// 錯誤訊息本體（不含位置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollError {
    Parse(String),
    DiceCountOutOfRange,
    FacesOutOfRange,
    FormulaTooLong,
    DivisionByZero,
    ExplodeCapExceeded(usize),
    MinTooLarge(i64),
    MaxTooSmall(i64),
    ValueOutOfRange,
    PoolLengthMismatch,
    CoinModifierNotSupported,
}

pub type Result<T> = std::result::Result<T, RollError>;

impl RollError {
    /// `Parse` 的訊息前綴（`錯誤：無法解析的公式：`）。
    ///
    /// 由 `Display` 推導而不是再寫一份字面——兩份遲早對不上。
    fn parse_prefix() -> String {
        Self::Parse(String::new()).to_string()
    }
}

/// 把公式裡出錯的那一段標起來，其餘用行內 code span 包住。
///
/// 形狀（使用者指定的格式）：
///
/// ```text
/// `1d4 + `***100dx<=6***` + 1d20 > 3000`
/// ```
///
/// - 出錯的部分包 `***`（Discord 粗斜體）
/// - 前後段各自包一個行內 code span——公式可能含 `*`，裸露會被 Discord
///   當成乘法／強調標記而吃字
/// - 出錯的部分本身含 `*` / `_` / `` ` `` 時**退化成只包 code span**：
///   那些字元會讖 `***` 破版，寧可沒有粗斜體也不要顯示錯亂
/// - span 不在字元邊界或整個落空時，整條包 code span（不硬切）
fn mark_span(src: &str, span: Span) -> String {
    let (start, end) = (span.start.min(src.len()), span.end.min(src.len()));
    let (Some(before), Some(bad), Some(after)) =
        (src.get(..start), src.get(start..end), src.get(end..))
    else {
        return code_span(src);
    };
    if bad.is_empty() {
        return code_span(src);
    }

    let mut out = String::new();
    if !before.is_empty() {
        out.push_str(&code_span(before));
    }
    if bad.contains(['*', '_', '`']) {
        out.push_str(&code_span(bad));
    } else {
        out.push_str("***");
        out.push_str(bad);
        out.push_str("***");
    }
    if !after.is_empty() {
        out.push_str(&code_span(after));
    }
    out
}

/// 行內 code span。
///
/// 圍欄比內容裡最長的反引號段多一個（CommonMark 規則），內容若以反引號
/// 開頭或結尾就補一個空格——否則 Discord 會把圍欄和內容連成一體而提早收尾。
///
/// 空內容回空字串：`` `` `` 不是合法的 code span，只會多出一對字面反引號。
fn code_span(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let run = text
        .chars()
        .fold((0usize, 0usize), |(best, run), c| {
            if c == '`' {
                (best.max(run + 1), run + 1)
            } else {
                (best, 0)
            }
        })
        .0;
    let fence = "`".repeat(run + 1);
    let pad = |s: &str| {
        if text.starts_with('`') || text.ends_with('`') {
            format!(" {s} ")
        } else {
            s.to_string()
        }
    };
    format!("{fence}{}{fence}", pad(text))
}

/// 原始碼位置（byte offset，相對於 [`crate::parser::strip_formula`] 之後的公式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[inline]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// 從單一位置擴張成「那一個詞」的範圍（到下一個空白為止）。
    pub fn word_at(src: &str, at: usize) -> Self {
        let bytes = src.as_bytes();
        if at >= bytes.len() {
            return Self::new(bytes.len(), bytes.len());
        }
        let mut end = at;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        Self::new(at, end)
    }
}

/// 帶位置的錯誤。
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedRollError {
    pub error: RollError,
    /// `None` 代表只知道「這條公式有問題」，指不出單一位置
    /// （例如執行期才發現的除以零、動態骰面超界）。
    pub span: Option<Span>,
    /// 剝離註解後的公式，用來畫指示箭頭。
    pub formula: Option<String>,
}

impl SpannedRollError {
    pub fn new(error: RollError) -> Self {
        Self {
            error,
            span: None,
            formula: None,
        }
    }

    pub fn at(error: RollError, span: Span) -> Self {
        Self {
            error,
            span: Some(span),
            formula: None,
        }
    }

    /// 補上公式（由 `engine` 在最外層填）。
    pub fn with_formula(mut self, formula: &str) -> Self {
        self.formula = Some(formula.to_string());
        self
    }

    /// 錯誤訊息本體（**與舊 Python 逐字相同**，不含任何標記）。
    pub fn message(&self) -> String {
        self.error.to_string()
    }

    /// 兩行指示：公式本體 + 對齊的插入符。沒有位置時回空字串。
    ///
    /// 插入符用**字元**位移，不是 byte 位移。這樣是安全的：`lexer` 只接受
    /// ASCII，所以失敗點之前必然是 ASCII；公式裡的 CJK 只會是「第一個非法
    /// 字元」，而它就是失敗點本身，前面不含多位元組字元。
    ///
    /// 超過 [`POINTER_MAX_WIDTH`] 的公式只顯示錯誤附近的視窗（前後加 `…`），
    /// 因為 Discord 訊息上限 2000 字元，公式上限 512 字元——照全放會撐爆。
    pub fn pointer(&self) -> String {
        let (Some(span), Some(formula)) = (self.span, self.formula.as_deref()) else {
            return String::new();
        };
        let chars: Vec<char> = formula.chars().collect();
        let Some(prefix) = formula.get(..span.start.min(formula.len())) else {
            return String::new();
        };
        let caret_at = prefix.chars().count();
        let underline = span
            .end
            .saturating_sub(span.start)
            .clamp(1, POINTER_MAX_WIDTH);

        let (view_start, view_end) = if chars.len() <= POINTER_MAX_WIDTH {
            (0, chars.len())
        } else {
            // 插入符偏左放，右邊留多一點上下文
            let start = caret_at.saturating_sub(POINTER_MAX_WIDTH / 3);
            (start, (start + POINTER_MAX_WIDTH).min(chars.len()))
        };

        let mut line: String = chars[view_start..view_end].iter().collect();
        let mut caret_pad = caret_at.saturating_sub(view_start);
        if view_start > 0 {
            line.insert(0, '…');
            caret_pad += 1;
        }
        if view_end < chars.len() {
            line.push('…');
        }
        format!("{line}\n{}{}", " ".repeat(caret_pad), "^".repeat(underline))
    }

    /// 給 Discord 看的完整訊息。
    ///
    /// **無法解析的公式**走行內格式：公式本身就嵌在訊息本體裡，出錯的部分
    /// 用粗斜體標出來（見 [`mark_span`]），因此不再附箭頭區塊——同一件事
    /// 不說兩次。
    ///
    /// 其他錯誤（語意錯誤）沿用「訊息 + 圍籬內的指示塊」。
    /// 兩邊都**一定要**把公式包住：公式可能含 `*` 或 `_`，裸露在 Discord
    /// 會被當成斜體／粗體標記而吃字。
    pub fn for_display(&self) -> String {
        if matches!(self.error, RollError::Parse(_)) {
            let echo = self.formula.as_deref().or(match &self.error {
                RollError::Parse(s) => Some(s.as_str()),
                _ => None,
            });
            let marked = match (self.span, echo) {
                (Some(span), Some(src)) => mark_span(src, span),
                // 指不出單一位置（語法合法但擲不出來）：整條包 code span 就好
                (_, Some(src)) => code_span(src),
                _ => String::new(),
            };
            return format!("{}{}", RollError::parse_prefix(), marked);
        }
        let pointer = self.pointer();
        if pointer.is_empty() {
            return self.message();
        }
        format!("{}\n\n```\n{pointer}\n```", self.message())
    }
}

/// `Display` 只給訊息本體（日誌／Rust 測試用）；Discord 要的多行指示版本
/// 請用 [`SpannedRollError::for_display`]。
impl std::fmt::Display for SpannedRollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for SpannedRollError {}

impl From<RollError> for SpannedRollError {
    fn from(error: RollError) -> Self {
        Self::new(error)
    }
}

impl From<SpannedRollError> for pyo3::PyErr {
    fn from(err: SpannedRollError) -> Self {
        let message = err.message();
        let pointer = err.pointer();
        let span = err.span.map(|s| (s.start, s.end));
        Python::attach(|py| {
            let exc = PyRollError::new_err(err.for_display());
            // 結構化欄位直接住在例外實例上，消費端不必再解析訊息字串。
            let value = exc.value(py);
            let _ = value.setattr("message", message);
            let _ = value.setattr("pointer", pointer);
            let _ = value.setattr("span", span);
            exc
        })
    }
}

/// 為 `RollError` 的 `Display` 實作使用 i18n 翻譯表。
impl std::fmt::Display for RollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 取得 variant 名稱
        let variant = match self {
            RollError::Parse(_) => "Parse",
            RollError::DiceCountOutOfRange => "DiceCountOutOfRange",
            RollError::FacesOutOfRange => "FacesOutOfRange",
            RollError::FormulaTooLong => "FormulaTooLong",
            RollError::DivisionByZero => "DivisionByZero",
            RollError::ExplodeCapExceeded(_) => "ExplodeCapExceeded",
            RollError::MinTooLarge(_) => "MinTooLarge",
            RollError::MaxTooSmall(_) => "MaxTooSmall",
            RollError::ValueOutOfRange => "ValueOutOfRange",
            RollError::PoolLengthMismatch => "PoolLengthMismatch",
            RollError::CoinModifierNotSupported => "CoinModifierNotSupported",
        };

        // 取得對應語言的樣板
        let lang = *CURRENT_LANG.lock().unwrap();
        let template = TRANSLATIONS
            .get(variant)
            .and_then(|map| map.get(&lang))
            .unwrap_or(&""); // 若找不到則回空字串（保守）

        // 替換 {0} 參數
        let formatted = match self {
            RollError::Parse(s) => template.replace("{0}", s),
            RollError::ExplodeCapExceeded(n) => template.replace("{0}", &n.to_string()),
            RollError::MinTooLarge(n) => template.replace("{0}", &n.to_string()),
            RollError::MaxTooSmall(n) => template.replace("{0}", &n.to_string()),
            _ => template.to_string(),
        };
        write!(f, "{}", formatted)
    }
}

impl Error for RollError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_span_wraps_in_a_single_backtick_pair() {
        assert_eq!(code_span("4d6"), "`4d6`");
        assert_eq!(code_span(""), "");
    }

    #[test]
    fn code_span_escapes_embedded_backticks_by_lengthening_the_fence() {
        // 內容裡有一個反引號 → 圍欄用兩個，前後補一個空格避免連成一體
        assert_eq!(code_span("a`b"), "``a`b``");
        assert_eq!(code_span("`x"), "`` `x ``");
        assert_eq!(code_span("x`"), "`` x` ``");
        assert_eq!(code_span("``"), "``` `` ```");
    }

    #[test]
    fn mark_span_bolditalics_the_bad_part_and_code_spans_the_rest() {
        // 使用者要求的形狀
        assert_eq!(
            mark_span("1d4 + 100dx<=6 + 1d20 > 3000", Span::new(6, 14)),
            "`1d4 + `***100dx<=6***` + 1d20 > 3000`"
        );
        // 出錯部分在開頭 / 結尾 / 整條：不產生空的 code span
        assert_eq!(
            mark_span("100dx<=6 + 1", Span::new(0, 8)),
            "***100dx<=6***` + 1`"
        );
        assert_eq!(mark_span("1d4+100dx", Span::new(4, 9)), "`1d4+`***100dx***");
        assert_eq!(mark_span("abc", Span::new(0, 3)), "***abc***");
    }

    #[test]
    fn mark_span_drops_the_emphasis_when_the_bad_part_would_break_it() {
        // `*` / `_` / 反引號會讓 *** 破版——寧可沒有粗斜體也不要顯示錯亂，
        // 出錯部分退化成 code span（前後段無論如何都被包住，`*` 不會被吃）
        assert_eq!(mark_span("1d4+2*3x", Span::new(4, 8)), "`1d4+``2*3x`");
        assert_eq!(mark_span("1d4+x_", Span::new(4, 6)), "`1d4+``x_`");
        assert_eq!(mark_span("1d4+x`", Span::new(4, 6)), "`1d4+``` x` ``");
        assert_eq!(mark_span("2*3dz", Span::new(3, 5)), "`2*3`***dz***");
    }

    #[test]
    fn mark_span_falls_back_to_a_plain_code_span_without_a_usable_span() {
        // 空 span（指在結尾）與不在字元邊界的 span 都不硬切
        assert_eq!(mark_span("abc", Span::new(1, 1)), "`abc`");
        assert_eq!(mark_span("註解abc", Span::new(0, 1)), "`註解abc`");
        assert_eq!(mark_span("abc", Span::new(9, 20)), "`abc`");
    }

    #[test]
    fn parse_prefix_comes_from_the_message_itself() {
        assert_eq!(RollError::parse_prefix(), "錯誤：無法解析的公式：");
        assert_eq!(
            RollError::Parse("1d4".into()).to_string(),
            "錯誤：無法解析的公式：1d4"
        );
    }

    #[test]
    fn parse_display_is_prefix_plus_markup() {
        let e = SpannedRollError::at(RollError::Parse("1d4+100dx<=6".into()), Span::new(4, 12))
            .with_formula("1d4+100dx<=6");
        assert_eq!(e.message(), "錯誤：無法解析的公式：1d4+100dx<=6");
        assert_eq!(
            e.for_display(),
            "錯誤：無法解析的公式：`1d4+`***100dx<=6***"
        );
    }

    #[test]
    fn parse_display_without_a_span_still_code_spans_the_formula() {
        let e = SpannedRollError::new(RollError::Parse("1d4+2*3".into())).with_formula("1d4+2*3");
        assert_eq!(e.for_display(), "錯誤：無法解析的公式：`1d4+2*3`");
    }

    #[test]
    fn parse_display_never_repeats_the_caret_block() {
        // 出錯部分已經在行內標出來，圍籬箭頭是同一件事說兩次
        let e = SpannedRollError::at(RollError::Parse("abc".into()), Span::new(0, 3))
            .with_formula("abc");
        assert_eq!(e.pointer(), "abc\n^^^", "pointer 仍保留（結構化欄位）");
        assert!(!e.for_display().contains("```"));
    }

    // i18n 測試
    #[test]
    fn i18n_switch_language() {
        // 預設中文
        assert_eq!(
            RollError::Parse("test".to_string()).to_string(),
            "錯誤：無法解析的公式：test"
        );
        // 切換英文
        set_language(Language::En);
        assert_eq!(
            RollError::Parse("test".to_string()).to_string(),
            "Error: Unable to parse formula: test"
        );
        // 再切回中文
        set_language(Language::ZhTW);
        assert_eq!(
            RollError::Parse("test".to_string()).to_string(),
            "錯誤：無法解析的公式：test"
        );
        // 其他錯誤類型也應切換
        set_language(Language::En);
        assert_eq!(
            RollError::DiceCountOutOfRange.to_string(),
            "Error: Dice count must be between 1 and 100"
        );
        set_language(Language::ZhTW);
        assert_eq!(
            RollError::DiceCountOutOfRange.to_string(),
            "錯誤：骰數必須介於 1~100"
        );
    }
}
