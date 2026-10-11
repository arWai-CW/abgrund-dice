//! Lexer: `logos` 掃描 → `SpannedToken` 串流（自帶 byte offset）。
//!
//! `SpannedToken` 同時是 winnow 的 token stream 元素：實作
//! [`winnow::stream::Location`]，讓 `TokenSlice` 的
//! `current_token_start()` / `previous_token_end()` 直接回傳**原始碼**的 byte
//! offset。parser 靠這個算出 `notation` / `mods_suffix`（原始碼切片）。

use crate::error::{RollError, Span, SpannedRollError};
use logos::Logos;
use winnow::stream::Location;

/// token 種類（無 payload；文字一律由 `span` 回切原始碼取得）。
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[logos(skip r"[ \t\r\n\f\v]+")]
pub enum TokenKind {
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(",")]
    Comma,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token(">=")]
    Ge,
    #[token("<=")]
    Le,
    #[token(">")]
    Gt,
    #[token("<")]
    Lt,
    #[token("=")]
    Eq,

    /// `123` 或 `1.5`（`\d+(\.\d+)?`，與舊 Python `_TOKEN_RE` 一致）。
    #[regex(r"[0-9]+(\.[0-9]+)?")]
    Number,

    /// `d` / `kh` / `dcx` / `floor` …，一律 `[A-Za-z]+`；語意由 parser 判讀。
    #[regex(r"[A-Za-z]+")]
    Ident,
}

impl TokenKind {
    pub fn is_compare(self) -> bool {
        matches!(self, Self::Ge | Self::Le | Self::Gt | Self::Lt | Self::Eq)
    }
}

/// 帶原始碼 byte offset 的 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpannedToken {
    pub kind: TokenKind,
    /// 原始碼起始 byte offset。
    pub start: u32,
    /// 原始碼結束 byte offset（exclusive）。
    pub end: u32,
}

impl SpannedToken {
    /// 回切此 token 的原始文字。
    pub fn text<'s>(&self, src: &'s str) -> &'s str {
        &src[self.start as usize..self.end as usize]
    }
}

/// winnow 需要：把 offset 轉成「原始碼位置」，而非 token index。
impl Location for SpannedToken {
    fn previous_token_end(&self) -> usize {
        self.end as usize
    }
    fn current_token_start(&self) -> usize {
        self.start as usize
    }
}

/// 掃描 `src`。任何無法匹配的位元組（含 CJK、`.` 之類的符號）→
/// `RollError::Parse`，訊息與舊 Python `_tokenize` 相同，並附上**第一個
/// 非法字元**的位置。
pub fn lex(src: &str) -> Result<Vec<SpannedToken>, SpannedRollError> {
    let mut lexer = TokenKind::lexer(src);
    let mut out = Vec::new();
    while let Some(res) = lexer.next() {
        let span = lexer.span();
        match res {
            Ok(kind) => out.push(SpannedToken {
                kind,
                start: span.start as u32,
                end: span.end as u32,
            }),
            Err(_) => {
                // logos 的錯誤 span 可能一路蓋到剩餘輸入，收斂成單一非法字元
                let start = span.start.min(src.len());
                let end = src[start..]
                    .chars()
                    .next()
                    .map_or(start, |c| start + c.len_utf8());
                return Err(SpannedRollError::at(
                    RollError::Parse(src.to_string()),
                    Span::new(start, end),
                ));
            }
        }
    }
    Ok(out)
}

/// 數字字面值：`\d+` 或 `\d+.\d+`。
///
/// 一律以 `f64` 承載（`/` 為真除法、`_fmt_num` 把整數值渲染成整數），
/// 與舊 Python `int | float` 的可觀察行為一致。超出 `f64` 精度的巨量整數
/// （>15 位有效數字）會失真——512 字元上限下屬理論邊界。
pub fn parse_number(text: &str) -> f64 {
    text.parse::<f64>().unwrap_or(f64::INFINITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn texts(src: &str) -> Vec<String> {
        lex(src)
            .unwrap()
            .iter()
            .map(|t| t.text(src).to_string())
            .collect()
    }

    #[test]
    fn basic_dice() {
        use TokenKind::*;
        assert_eq!(kinds("4d6k3"), [Number, Ident, Number, Ident, Number]);
        assert_eq!(texts("4d6k3"), ["4", "d", "6", "k", "3"]);
    }

    #[test]
    fn pool_and_compare() {
        use TokenKind::*;
        assert_eq!(
            kinds("{4d6, 3d8}kh"),
            [LBrace, Number, Ident, Number, Comma, Number, Ident, Number, RBrace, Ident]
        );
        assert_eq!(
            kinds("1d20+2>=15"),
            [Number, Ident, Number, Plus, Number, Ge, Number]
        );
    }

    #[test]
    fn ident_is_greedy_letters() {
        // `dfcs` 是「單一」ident，不是 df + cs —— 與舊 Python `[A-Za-z]+` 相同。
        assert_eq!(texts("4dfcs"), ["4", "dfcs"]);
        assert_eq!(texts("4dF"), ["4", "dF"]);
        assert_eq!(texts("4dcc1"), ["4", "dcc", "1"]);
    }

    #[test]
    fn decimal_number() {
        assert_eq!(parse_number("2.7"), 2.7);
        assert_eq!(texts("round(1d1*2.7)"), [
            "round", "(", "1", "d", "1", "*", "2.7", ")"
        ]);
    }

    #[test]
    fn bare_dot_is_lex_error() {
        // `4.d6` → 舊 Python 也無法解析（`.` 不在 token 集合內）
        let e = lex("4.d6").unwrap_err();
        assert!(matches!(e.error, RollError::Parse(_)));
        assert_eq!(e.span, Some(Span::new(1, 2)), "應指向 `.`");
    }

    #[test]
    fn cjk_is_lex_error_pointing_at_the_first_bad_char() {
        let e = lex("2d6 註解").unwrap_err();
        assert!(matches!(e.error, RollError::Parse(_)));
        // `2d6 ` 是 4 個 byte，`註` 佔 3 個 byte
        assert_eq!(e.span, Some(Span::new(4, 7)));
    }

    #[test]
    fn whitespace_is_skipped() {
        assert_eq!(kinds("  1 \t+\n2 "), [
            TokenKind::Number,
            TokenKind::Plus,
            TokenKind::Number
        ]);
    }

    #[test]
    fn locations_are_source_offsets() {
        let src = "12+345";
        let toks = lex(src).unwrap();
        assert_eq!((toks[0].start, toks[0].end), (0, 2));
        assert_eq!((toks[1].start, toks[1].end), (2, 3));
        assert_eq!((toks[2].start, toks[2].end), (3, 6));
        assert_eq!(toks[2].text(src), "345");
    }
}
