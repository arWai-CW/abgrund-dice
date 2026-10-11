//! winnow parser：token stream → AST。
//!
//! 分層與舊 Python `_Parser` 一致（語意 oracle）：
//!
//! ```text
//! roll   := expr (cmp num)? EOF
//! expr   := term (('+' | '-') term)*          // winnow repeat().fold()
//! term   := unary (('*' | '/') unary)*        // winnow repeat().fold()
//! unary  := '-' atom | atom
//! atom   := num | ident | '(' expr ')' | '{' expr (',' expr)* '}' pool_mods
//!         ; num / ')' 之後若接 d|df|dF|dc* → 骰子產生式
//! ```
//!
//! `notation` / `mods_suffix` 由 [`winnow::stream::Location`] 回報的**原始碼
//! offset** 切片取得，因此渲染結果與使用者輸入逐字元一致。

use crate::ast::*;
use crate::error::{RollError, Span, SpannedRollError};
use crate::lexer::{lex, parse_number, SpannedToken, TokenKind};
use crate::types::*;
use std::cell::Cell;
use winnow::combinator::{repeat, separated};
use winnow::error::{ErrMode, ParserError};
use winnow::stream::{Location, Stream, TokenSlice};
use winnow::Parser;

pub type Tokens<'i> = TokenSlice<'i, SpannedToken>;
pub type PResult<T> = Result<T, ErrMode<ParseFailure>>;

/// 解析失敗的兩種語意。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseFailure {
    /// 此產生式不匹配，可回溯（最終一律轉成「無法解析的公式」）。
    /// `at` 是解析卡住的原始碼位置。
    Nom { at: usize },
    /// 語法成立但語意非法（超界、硬幣不支援修飾詞…），不可回溯。
    Fatal { error: RollError, span: Span },
}

impl<'i> ParserError<Tokens<'i>> for ParseFailure {
    type Inner = Self;

    fn from_input(input: &Tokens<'i>) -> Self {
        ParseFailure::Nom {
            at: input.current_token_start(),
        }
    }

    fn into_inner(self) -> Result<Self::Inner, Self> {
        Ok(self)
    }

    fn is_backtrack(&self) -> bool {
        matches!(self, ParseFailure::Nom { .. })
    }

    /// 合併兩個分支的錯誤時，保留**走得最遠**的位置——那最接近真正的錯誤點。
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (ParseFailure::Nom { at: a }, ParseFailure::Nom { at: b }) => {
                ParseFailure::Nom { at: a.max(b) }
            }
            (f @ ParseFailure::Fatal { .. }, _) => f,
            (_, f @ ParseFailure::Fatal { .. }) => f,
        }
    }
}

/// Python `_is_int_in`：必須是「整數值」且落在 `[lo, hi]`。
fn is_int_in(v: f64, lo: f64, hi: f64) -> bool {
    v.is_finite() && v.fract() == 0.0 && v >= lo && v <= hi
}

/// Python `int(v)`：向零截斷（Rust `as` 對 inf/NaN 會飽和，符合「夾住」語意）。
fn trunc_i64(v: f64) -> i64 {
    v.trunc() as i64
}

fn compare_op(k: TokenKind) -> Option<CompareOp> {
    match k {
        TokenKind::Ge => Some(CompareOp::Ge),
        TokenKind::Gt => Some(CompareOp::Gt),
        TokenKind::Le => Some(CompareOp::Le),
        TokenKind::Lt => Some(CompareOp::Lt),
        TokenKind::Eq => Some(CompareOp::Eq),
        _ => None,
    }
}

/// 骰數：字面值（待驗證）或動態運算式。
enum RawCount {
    Lit { value: f64, span: Span },
    Dyn(Expr),
}

/// 骰面：字面值（待驗證）、動態運算式，或 fate。
enum RawFaces {
    Fate,
    Lit { num: Num, span: Span },
    Dyn(Expr),
}

/// 先做 `len > 512` 檢查、再去註解與空白；回傳可供切片 `notation` 的來源字串。
pub fn strip_formula(formula: &str) -> Result<&str, SpannedRollError> {
    if formula.len() > MAX_FORMULA_LEN {
        // 指示箭頭指向「開始超出上限」的位置
        return Err(SpannedRollError::at(
            RollError::FormulaTooLong,
            Span::new(MAX_FORMULA_LEN, MAX_FORMULA_LEN + 1),
        )
        .with_formula(formula));
    }
    let src = formula.split('#').next().unwrap_or("").trim();
    if src.is_empty() {
        return Err(SpannedRollError::new(RollError::Parse(formula.to_string())).with_formula(formula));
    }
    Ok(src)
}

/// lex + parse。`src` 必須是 [`strip_formula`] 的輸出（`notation` 靠它切片）。
pub fn parse(src: &str) -> Result<ParsedRoll, SpannedRollError> {
    let tokens = lex(src)?;
    let mut input = Tokens::new(&tokens);
    let ctx = ParserCtx {
        src,
        depth: Cell::new(0),
    };
    match ctx.roll(&mut input) {
        Ok(parsed) => Ok(parsed),
        // 語意錯誤：parser 已經知道精確位置
        Err(ErrMode::Cut(ParseFailure::Fatal { error, span })) => {
            Err(SpannedRollError::at(error, span))
        }
        // 解析失敗：`at` 是卡住的位置，用它畫指示箭頭
        Err(ErrMode::Backtrack(ParseFailure::Nom { at })) => Err(SpannedRollError::at(
            RollError::Parse(src.to_string()),
            Span::word_at(src, at),
        )),
        Err(_) => Err(SpannedRollError::new(RollError::Parse(src.to_string()))),
    }
}

pub struct ParserCtx<'src> {
    src: &'src str,
    /// `(` / `{` 的遞迴深度上限，避免 stack overflow（Python `_MAX_PARSE_DEPTH`）。
    depth: Cell<u32>,
}

impl<'src> ParserCtx<'src> {
    // ---------------------------------------------------------------- terminals

    #[inline]
    fn peek<'i>(&self, input: &Tokens<'i>) -> Option<&'i SpannedToken> {
        input.peek_token()
    }

    #[inline]
    fn bump<'i>(&self, input: &mut Tokens<'i>) -> PResult<&'i SpannedToken> {
        input.next_token().ok_or_else(|| self.fail(input))
    }

    #[inline]
    fn expect<'i>(&self, input: &mut Tokens<'i>, kind: TokenKind) -> PResult<&'i SpannedToken> {
        match input.next_token() {
            Some(t) if t.kind == kind => Ok(t),
            _ => Err(self.fail(input)),
        }
    }

    /// 目前 token 是 ident 就回傳其文字（不消耗）。
    #[inline]
    fn peek_ident<'i>(&self, input: &Tokens<'i>) -> Option<&'src str> {
        self.peek(input)
            .filter(|t| t.kind == TokenKind::Ident)
            .map(|t| t.text(self.src))
    }

    /// 目前 token 是 num 就回傳其字面值（不消耗）。保留 Python int/float 身分。
    #[inline]
    fn peek_num<'i>(&self, input: &Tokens<'i>) -> Option<Num> {
        self.peek(input)
            .filter(|t| t.kind == TokenKind::Number)
            .and_then(|t| self.num_lit(t).ok())
    }

    /// 目前 token 的 ident 文字與位置（不消耗）。
    #[inline]
    fn peek_ident_tok<'i>(&self, input: &Tokens<'i>) -> Option<&'i SpannedToken> {
        self.peek(input).filter(|t| t.kind == TokenKind::Ident)
    }

    /// 數字字面值 → [`Num`]。
    ///
    /// **刻意與舊 Python 不同**：整數字面值若無法被 `f64` 精確表示（> 2^53，
    /// 即 16 位以上十進位數字），一律拒收成「無法解析」。
    ///
    /// 舊版用 Python 大整數，`9999999999999999` 會原樣保留；Rust 端以 `f64`
    /// 承載數值，若照收會**靜默**變成 `10000000000000000`。這裡選擇大聲拒絕：
    /// 這個文法裡所有數值欄位都被夾在 1~100000，16 位數字不可能是合法輸入。
    /// 帶小數點的字面值不受影響（Python 也走 `float()`，精度行為相同）。
    fn num_lit(&self, tok: &SpannedToken) -> PResult<Num> {
        let text = tok.text(self.src);
        if !text.contains('.') {
            let exact = text.parse::<i64>().is_ok_and(|v| v as f64 as i64 == v);
            if !exact {
                return Err(ErrMode::Backtrack(ParseFailure::Nom {
                    at: tok.start as usize,
                }));
            }
        }
        Ok(Num::literal(text, parse_number(text)))
    }

    /// Python `_cp_ahead`：比較運算子或裸數字都算 CP 開頭。
    #[inline]
    fn cp_ahead<'i>(&self, input: &Tokens<'i>) -> bool {
        self.peek(input)
            .is_some_and(|t| t.kind.is_compare() || t.kind == TokenKind::Number)
    }

    /// 原始碼 byte offset：目前位置（供 `notation` 起點）。
    #[inline]
    fn start_offset<'i>(&self, input: &Tokens<'i>) -> usize {
        input.current_token_start()
    }

    /// 原始碼 byte offset：已消耗 token 的結尾（供 `notation` 終點）。
    #[inline]
    fn end_offset<'i>(&self, input: &Tokens<'i>) -> usize {
        input.previous_token_end()
    }

    /// 解析卡住：回報目前位置。
    #[inline]
    fn fail<'i>(&self, input: &Tokens<'i>) -> ErrMode<ParseFailure> {
        ErrMode::Backtrack(ParseFailure::Nom {
            at: self.start_offset(input),
        })
    }

    /// 語意錯誤：附上出錯 token 的位置。
    #[inline]
    fn fatal_at(&self, error: RollError, span: Span) -> ErrMode<ParseFailure> {
        ErrMode::Cut(ParseFailure::Fatal { error, span })
    }

    /// 目前 token 的範圍（供錯誤指示）。
    #[inline]
    fn span_here<'i>(&self, input: &Tokens<'i>) -> Span {
        match self.peek(input) {
            Some(t) => Span::new(t.start as usize, t.end as usize),
            None => Span::new(self.end_offset(input), self.end_offset(input)),
        }
    }

    fn notation(&self, start: usize, end: usize) -> String {
        self.src.get(start..end).unwrap_or("").to_string()
    }

    /// 進入一層 `(` / `{`；超過上限即視為解析失敗（Python 會拋 RollError）。
    fn nested<'i, F>(&self, input: &mut Tokens<'i>, f: F) -> PResult<Expr>
    where
        F: FnOnce(&mut Tokens<'i>) -> PResult<Expr>,
    {
        if self.depth.get() as usize >= MAX_PARSE_DEPTH {
            return Err(self.fail(input));
        }
        self.depth.set(self.depth.get() + 1);
        let out = f(input);
        self.depth.set(self.depth.get() - 1);
        out
    }

    // ---------------------------------------------------------------- grammar

    fn roll<'i>(&self, input: &mut Tokens<'i>) -> PResult<ParsedRoll> {
        let expr = self.expr(input)?;
        // `_CHECK_OPS` 不含 `=`（`1d20=15` 屬無法解析）
        let check = match self.peek(input).and_then(|t| compare_op(t.kind)) {
            Some(op) if !matches!(op, CompareOp::Eq) => {
                self.bump(input)?;
                let target = self.peek_num(input).ok_or_else(|| self.fail(input))?;
                self.bump(input)?;
                Some(CompareCondition { op, target })
            }
            _ => None,
        };
        if self.peek(input).is_some() {
            return Err(self.fail(input));
        }
        Ok(ParsedRoll { expr, check })
    }

    fn expr<'i>(&self, input: &mut Tokens<'i>) -> PResult<Expr> {
        let init = self.term(input)?;
        repeat(
            0..,
            (
                |i: &mut Tokens<'i>| self.add_sub(i),
                |i: &mut Tokens<'i>| self.term(i),
            ),
        )
        .fold(
            move || init.clone(),
            |acc, (op, rhs): (TokenKind, Expr)| Expr::BinOp {
                op: if op == TokenKind::Plus {
                    BinOp::Add
                } else {
                    BinOp::Sub
                },
                left: Box::new(acc),
                right: Box::new(rhs),
            },
        )
        .parse_next(input)
    }

    fn term<'i>(&self, input: &mut Tokens<'i>) -> PResult<Expr> {
        let init = self.unary(input)?;
        repeat(
            0..,
            (
                |i: &mut Tokens<'i>| self.mul_div(i),
                |i: &mut Tokens<'i>| self.unary(i),
            ),
        )
        .fold(
            move || init.clone(),
            |acc, (op, rhs): (TokenKind, Expr)| Expr::BinOp {
                op: if op == TokenKind::Star {
                    BinOp::Mul
                } else {
                    BinOp::Div
                },
                left: Box::new(acc),
                right: Box::new(rhs),
            },
        )
        .parse_next(input)
    }

    fn add_sub<'i>(&self, input: &mut Tokens<'i>) -> PResult<TokenKind> {
        match self.peek(input).map(|t| t.kind) {
            Some(k @ (TokenKind::Plus | TokenKind::Minus)) => {
                self.bump(input)?;
                Ok(k)
            }
            _ => Err(self.fail(input)),
        }
    }

    fn mul_div<'i>(&self, input: &mut Tokens<'i>) -> PResult<TokenKind> {
        match self.peek(input).map(|t| t.kind) {
            Some(k @ (TokenKind::Star | TokenKind::Slash)) => {
                self.bump(input)?;
                Ok(k)
            }
            _ => Err(self.fail(input)),
        }
    }

    /// 只允許**一層**前置負號（`--5` 在舊 Python 也不合法）。
    fn unary<'i>(&self, input: &mut Tokens<'i>) -> PResult<Expr> {
        if self.peek(input).map(|t| t.kind) == Some(TokenKind::Minus) {
            self.bump(input)?;
            return Ok(Expr::Neg(Box::new(self.atom(input)?)));
        }
        self.atom(input)
    }

    fn atom<'i>(&self, input: &mut Tokens<'i>) -> PResult<Expr> {
        let start = self.start_offset(input);
        let Some(tok) = self.peek(input) else {
            return Err(self.fail(input));
        };
        match tok.kind {
            TokenKind::Number => {
                let lit = self.num_lit(tok)?;
                let span = Span::new(tok.start as usize, tok.end as usize);
                self.bump(input)?;
                // `4d6` / `4df` / `4dc` 之後才算是骰子，否則只是純數字
                match self.dice_suffix(input, RawCount::Lit { value: lit.value, span }, start)? {
                    Some(expr) => Ok(expr),
                    None => {
                        self.botched_dice(input, start)?;
                        Ok(Expr::Num(lit))
                    }
                }
            }
            TokenKind::Ident => {
                let name = tok.text(self.src);
                if let Some(kind) = FuncKind::parse(name) {
                    self.bump(input)?;
                    self.expect(input, TokenKind::LParen)?;
                    let arg = self.nested(input, |i| self.expr(i))?;
                    self.expect(input, TokenKind::RParen)?;
                    return Ok(Expr::Func {
                        kind,
                        arg: Box::new(arg),
                    });
                }
                match dice_ident(name) {
                    // 裸 `d20` / `df` / `dc`：骰數預設 1
                    Some((fate, true)) => {
                        let span = Span::new(tok.start as usize, tok.end as usize);
                        self.bump(input)?;
                        self.dice_body(input, RawCount::Lit { value: 1.0, span }, start, fate)
                            .map(Expr::Dice)
                    }
                    Some((_, false)) => {
                        let span = Span::new(tok.start as usize, tok.end as usize);
                        self.bump(input)?;
                        self.coin_body(input, RawCount::Lit { value: 1.0, span }, start, tok)
                            .map(Expr::Dice)
                    }
                    None => Err(self.fail(input)),
                }
            }
            TokenKind::LParen => {
                self.bump(input)?;
                let inner = self.nested(input, |i| self.expr(i))?;
                self.expect(input, TokenKind::RParen)?;
                match self.dice_suffix(input, RawCount::Dyn(inner.clone()), start)? {
                    Some(expr) => Ok(expr),
                    None => {
                        self.botched_dice(input, start)?;
                        Ok(inner)
                    }
                }
            }
            TokenKind::LBrace => self.pool(input, start),
            _ => Err(self.fail(input)),
        }
    }

    /// `num` / `(expr)` 之後的骰子後綴：`d…` / `df` / `dF` / `dc…`。
    /// 沒有後綴時回 `Ok(None)`（呼叫端保留原本的節點）。
    fn dice_suffix<'i>(
        &self,
        input: &mut Tokens<'i>,
        count: RawCount,
        start: usize,
    ) -> PResult<Option<Expr>> {
        let Some(tok) = self.peek_ident_tok(input) else {
            return Ok(None);
        };
        let Some((fate, is_dice)) = dice_ident(tok.text(self.src)) else {
            return Ok(None);
        };
        self.bump(input)?;
        let node = if is_dice {
            self.dice_body(input, count, start, fate)?
        } else {
            self.coin_body(input, count, start, tok)?
        };
        Ok(Some(Expr::Dice(node)))
    }

    /// 數字後面接了「想當骰子但字母不對」的 ident（`100dx`、`4fdx`、`2 ducks`）。
    ///
    /// 這種輸入一律是語法錯誤——`roll` 的 EOF 檢查本來就會擋。但舊做法只指出
    /// 落單的那個 ident，看不出問題是從哪個數字開始的（`100dx` 只報 `dx`，
    /// 沒有空白時甚至整條尾巴都被當成出錯範圍）。這裡把**整個骰子項**——
    /// 數字 + ident + 後面的比較條件——當成出錯範圍。
    ///
    /// 用 `Cut` 而不是 `fail()`：位置比「解析到一半才發現有剩餘」精確，
    /// 也不會被 `repeat` 當成可回溯而吞掉。
    fn botched_dice<'i>(&self, input: &mut Tokens<'i>, start: usize) -> PResult<()> {
        let Some(ident) = self.peek_ident_tok(input).copied() else {
            return Ok(()); // 數字後面不是 ident：正常結尾（EOF、運算子、`*`…）
        };
        let mut end = ident.end as usize;
        self.bump(input)?;
        // `<=6` / `>=3` 之類的條件是附屬在這個骰子項上的，一起指出來
        if self.peek(input).is_some_and(|t| t.kind.is_compare()) {
            self.bump(input)?;
            if let Some(num) = self
                .peek(input)
                .filter(|t| t.kind == TokenKind::Number)
                .copied()
            {
                end = num.end as usize;
                self.bump(input)?;
            }
        }
        Err(self.fatal_at(
            RollError::Parse(self.src.to_string()),
            Span::new(start, end),
        ))
    }

    /// 骰面：`f`/`F`（fate）、`-`? (`num` | `(expr)`)。
    fn parse_faces<'i>(&self, input: &mut Tokens<'i>) -> PResult<(bool, RawFaces)> {
        if matches!(self.peek_ident(input), Some("f") | Some("F")) {
            self.bump(input)?;
            return Ok((true, RawFaces::Fate));
        }
        let mut sign = 1.0;
        if self.peek(input).map(|t| t.kind) == Some(TokenKind::Minus) {
            self.bump(input)?;
            sign = -1.0;
        }
        if self.peek(input).map(|t| t.kind) == Some(TokenKind::LParen) {
            self.bump(input)?;
            let inner = self.nested(input, |i| self.expr(i))?;
            self.expect(input, TokenKind::RParen)?;
            let e = if sign < 0.0 {
                Expr::Neg(Box::new(inner))
            } else {
                inner
            };
            return Ok((false, RawFaces::Dyn(e)));
        }
        let span = self.span_here(input);
        let Some(v) = self.peek_num(input) else {
            return Err(self.fail(input));
        };
        self.bump(input)?;
        // Python `sign * t.val`：int 乘 float 仍是 float
        Ok((
            false,
            RawFaces::Lit {
                num: Num {
                    value: v.value * sign,
                    is_float: v.is_float,
                },
                span,
            },
        ))
    }

    /// `NdX` 本體：骰面 → 驗證（骰數先於骰面）→ 修飾詞 → notation。
    fn dice_body<'i>(
        &self,
        input: &mut Tokens<'i>,
        raw_count: RawCount,
        start: usize,
        fate_marker: bool,
    ) -> PResult<DiceNode> {
        let (fate, raw_faces) = if fate_marker {
            (true, RawFaces::Fate)
        } else {
            self.parse_faces(input)?
        };

        // 順序同 Python `_validate_dice`：先骰數、後骰面
        let count = self.resolve_count(raw_count)?;
        let faces = if fate {
            DiceFaces::Fate
        } else {
            self.resolve_faces(raw_faces)?
        };

        // fate 產生式不吃修飾詞（`4dfcs` 因此無法解析）
        // 動態骰面 → None（值域未知，延到執行期驗證）
        let domain = match faces {
            DiceFaces::Static(f) => Some(DiceDomain::normal(f)),
            _ => None,
        };
        let mods = if fate {
            Vec::new()
        } else {
            self.parse_mods(input, domain)?
        };
        let _end = self.end_offset(input);

        let end = self.end_offset(input);
        Ok(DiceNode {
            count,
            faces,
            kind: if fate {
                DiceKind::Fate
            } else {
                DiceKind::Normal
            },
            mods,
            notation: self.notation(start, end),
        })
    }

    /// `Ndc` / `Ndcc0` / `Ndcc1`。不支援的字母（`4dcx`）先報錯，再驗證骰數。
    fn coin_body<'i>(
        &self,
        input: &mut Tokens<'i>,
        raw_count: RawCount,
        start: usize,
        ident: &SpannedToken,
    ) -> PResult<DiceNode> {
        let ident_val = ident.text(self.src);
        let ident_span = Span::new(ident.start as usize, ident.end as usize);
        let coin_call: u8 = if ident_val == "dc" {
            1
        } else if ident_val == "dcc" {
            let Some(v) = self.peek_num(input) else {
                return Err(self.fail(input));
            };
            if v.value != 0.0 && v.value != 1.0 {
                return Err(self.fail(input));
            }
            self.bump(input)?;
            v.value as u8
        } else {
            return Err(self.fatal_at(RollError::CoinModifierNotSupported, ident_span));
        };

        let count = self.resolve_count(raw_count)?;
        let mods = self.parse_mods(input, Some(DiceDomain::coin()))?;
        for m in &mods {
            if matches!(
                m,
                DiceMod::KeepDrop { .. } | DiceMod::Reroll { .. } | DiceMod::Explode { .. }
            ) {
                return Err(self.fatal_at(RollError::CoinModifierNotSupported, ident_span));
            }
        }
        let end = self.end_offset(input);
        Ok(DiceNode {
            count,
            faces: DiceFaces::Static(0),
            kind: DiceKind::Coin { call: coin_call },
            mods,
            notation: self.notation(start, end),
        })
    }

    fn resolve_count(&self, raw: RawCount) -> PResult<DiceCount> {
        match raw {
            RawCount::Dyn(e) => Ok(DiceCount::Dynamic(Box::new(e))),
            RawCount::Lit { value, span } => {
                if !is_int_in(value, 1.0, 100.0) {
                    return Err(self.fatal_at(RollError::DiceCountOutOfRange, span));
                }
                Ok(DiceCount::Static(value as u32))
            }
        }
    }

    fn resolve_faces(&self, raw: RawFaces) -> PResult<DiceFaces> {
        match raw {
            RawFaces::Fate => Ok(DiceFaces::Fate),
            RawFaces::Dyn(e) => Ok(DiceFaces::Dynamic(Box::new(e))),
            RawFaces::Lit { num, span } => {
                if !is_int_in(num.value, 1.0, 100_000.0) {
                    return Err(self.fatal_at(RollError::FacesOutOfRange, span));
                }
                Ok(DiceFaces::Static(num.value as u32))
            }
        }
    }

    fn parse_mods<'i>(
        &self,
        input: &mut Tokens<'i>,
        domain: Option<DiceDomain>,
    ) -> PResult<Vec<DiceMod>> {
        repeat(0.., |i: &mut Tokens<'i>| self.dice_mod(i, domain)).parse_next(input)
    }

    /// 單一骰子修飾詞；遇到非修飾詞的 ident 一律**不消耗**並回 `Nom`，
    /// 讓 `repeat` 收斂後由 `roll` 的 EOF 檢查報錯。
    ///
    /// `domain` 是骰值合法範圍；動態骰面時為 `None`，`min`/`max` 的檢查
    /// 延到執行期（見 `evaluator`）。
    fn dice_mod<'i>(
        &self,
        input: &mut Tokens<'i>,
        domain: Option<DiceDomain>,
    ) -> PResult<DiceMod> {
        let Some(name) = self.peek_ident(input) else {
            return Err(self.fail(input));
        };
        match name {
            "kh" | "k" | "kl" | "dh" | "dl" | "d" => {
                self.bump(input)?;
                let n = match self.peek_num(input) {
                    Some(v) => {
                        self.bump(input)?;
                        trunc_i64(v.value)
                    }
                    None => 1,
                };
                Ok(DiceMod::KeepDrop {
                    kind: KeepDropKind::parse(name).expect("known keep/drop id"),
                    n,
                })
            }
            "r" | "rr" => {
                self.bump(input)?;
                let mut conditions = Vec::new();
                while self.cp_ahead(input) {
                    conditions.push(self.parse_cp(input)?);
                }
                Ok(DiceMod::Reroll {
                    recursive: name == "rr",
                    conditions,
                })
            }
            "x" | "xo" => {
                self.bump(input)?;
                let cap = if name == "xo" {
                    Some(1)
                } else {
                    let cap_span = self.span_here(input);
                    match self.peek_num(input) {
                        Some(v) => {
                            self.bump(input)?;
                            if !is_int_in(v.value, 0.0, MAX_EXPLODE_DICE as f64) {
                                return Err(self.fatal_at(
                                    RollError::ExplodeCapExceeded(MAX_EXPLODE_DICE),
                                    cap_span,
                                ));
                            }
                            Some(v.value as u32)
                        }
                        None => None,
                    }
                };
                let condition = if self.cp_ahead(input) {
                    Some(self.parse_cp(input)?)
                } else {
                    None
                };
                Ok(DiceMod::Explode { condition, cap })
            }
            "min" | "max" => {
                self.bump(input)?;
                let kind = MinMaxKind::parse(name).expect("known min/max id");
                let bound_span = self.span_here(input);
                let bound = match self.peek_num(input) {
                    Some(v) => {
                        self.bump(input)?;
                        // 全域合理性先擋（順序與 oracle 一致：`4d6min100001` 仍報
                        // 「數值必須介於 0~100000」），再擋值域。
                        if !is_int_in(v.value, 0.0, 100_000.0) {
                            return Err(self.fatal_at(RollError::ValueOutOfRange, bound_span));
                        }
                        if let Some(d) = domain {
                            if let Some(e) = d.minmax_error(kind, v.value as i64) {
                                return Err(self.fatal_at(e, bound_span));
                            }
                        }
                        Some(v.value as u32)
                    }
                    None => None,
                };
                Ok(DiceMod::MinMax { kind, bound })
            }
            "ms" => {
                self.bump(input)?;
                let ms_span = self.span_here(input);
                let Some(v) = self.peek_num(input) else {
                    return Err(self.fail(input));
                };
                self.bump(input)?;
                if !is_int_in(v.value, 0.0, 100_000.0) {
                    return Err(self.fatal_at(RollError::ValueOutOfRange, ms_span));
                }
                Ok(DiceMod::MarginSuccess(v.value as u32))
            }
            "even" | "odd" => {
                self.bump(input)?;
                Ok(DiceMod::Count {
                    kind: CountKind::parse(name).expect("known parity id"),
                    condition: None,
                })
            }
            "cs" | "cf" | "df" | "sf" => {
                self.bump(input)?;
                let condition = if self.cp_ahead(input) {
                    Some(self.parse_cp(input)?)
                } else {
                    None
                };
                if let Some(kind) = CountKind::parse(name) {
                    Ok(DiceMod::Count { kind, condition })
                } else {
                    Ok(DiceMod::Deduct {
                        kind: DeductKind::parse(name).expect("known deduct id"),
                        condition,
                    })
                }
            }
            _ => Err(self.fail(input)),
        }
    }

    /// CP：`(cmp num)? | num`；裸數字隱含 `=`。
    fn parse_cp<'i>(&self, input: &mut Tokens<'i>) -> PResult<CompareCondition> {
        let Some(tok) = self.peek(input) else {
            return Err(self.fail(input));
        };
        match tok.kind {
            k if k.is_compare() => {
                let op = compare_op(k).ok_or_else(|| self.fail(input))?;
                self.bump(input)?;
                let target = self.peek_num(input).ok_or_else(|| self.fail(input))?;
                self.bump(input)?;
                Ok(CompareCondition { op, target })
            }
            TokenKind::Number => {
                let target = self.num_lit(tok)?;
                self.bump(input)?;
                Ok(CompareCondition {
                    op: CompareOp::Eq,
                    target,
                })
            }
            _ => Err(self.fail(input)),
        }
    }

    /// `{ term, term, … }` + 池層修飾詞（`kh` / `cs` / `even` …）。
    fn pool<'i>(&self, input: &mut Tokens<'i>, _start: usize) -> PResult<Expr> {
        self.expect(input, TokenKind::LBrace)?;
        let terms = self.nested_terms(input)?;
        self.expect(input, TokenKind::RBrace)?;
        let brace_end = self.end_offset(input);
        let mods = self.parse_pool_mods(input)?;
        let end = self.end_offset(input);
        Ok(Expr::Pool(PoolNode {
            terms,
            mods,
            mods_suffix: self.notation(brace_end, end),
        }))
    }

    fn nested_terms<'i>(&self, input: &mut Tokens<'i>) -> PResult<Vec<Expr>> {
        if self.depth.get() as usize >= MAX_PARSE_DEPTH {
            return Err(self.fail(input));
        }
        self.depth.set(self.depth.get() + 1);
        let out = separated(
            1..,
            |i: &mut Tokens<'i>| self.expr(i),
            |i: &mut Tokens<'i>| self.expect(i, TokenKind::Comma),
        )
        .parse_next(input);
        self.depth.set(self.depth.get() - 1);
        out
    }

    /// 池層修飾詞只認 `kh k kl dh dl d cs cf even odd`；其他 ident 不消耗。
    fn parse_pool_mods<'i>(&self, input: &mut Tokens<'i>) -> PResult<Vec<PoolMod>> {
        repeat(0.., |i: &mut Tokens<'i>| self.pool_mod(i)).parse_next(input)
    }

    fn pool_mod<'i>(&self, input: &mut Tokens<'i>) -> PResult<PoolMod> {
        let Some(name) = self.peek_ident(input) else {
            return Err(self.fail(input));
        };
        match name {
            "kh" | "k" | "kl" | "dh" | "dl" | "d" => {
                self.bump(input)?;
                let n = match self.peek_num(input) {
                    Some(v) => {
                        self.bump(input)?;
                        trunc_i64(v.value)
                    }
                    None => 1,
                };
                Ok(PoolMod::KeepDrop {
                    kind: KeepDropKind::parse(name).expect("known keep/drop id"),
                    n,
                })
            }
            "even" | "odd" => {
                self.bump(input)?;
                Ok(PoolMod::Count {
                    kind: CountKind::parse(name).expect("known parity id"),
                    condition: None,
                })
            }
            "cs" | "cf" => {
                self.bump(input)?;
                let condition = if self.cp_ahead(input) {
                    Some(self.parse_cp(input)?)
                } else {
                    None
                };
                Ok(PoolMod::Count {
                    kind: CountKind::parse(name).expect("known count id"),
                    condition,
                })
            }
            _ => Err(self.fail(input)),
        }
    }
}

/// 骰子 ident → `(fate, is_dice)`。`None` = 不是骰子產生式。（`Expect` 保持 `'src`）
fn dice_ident(name: &str) -> Option<(bool, bool)> {
    match name {
        "d" => Some((false, true)),
        "df" | "dF" => Some((true, true)),
        _ if name.starts_with("dc") => Some((false, false)),
        _ => None,
    }
}
