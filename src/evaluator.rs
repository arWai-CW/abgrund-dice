//! AST 評估器：執行骰子 + 產生渲染字串。
//!
//! 舊 Python 是「兩趟」——`_roll_tree` 先擲完所有骰子，`_eval` 再算值與字串。
//! 兩趟的走訪順序完全相同（left-to-right、inner-first），且 `_eval` 不消耗
//! RNG，因此這裡合併成**單趟**：同樣的 RNG 消耗順序、同樣的渲染輸出。
//!
//! 所有數值以 `f64` 承載（`/` 為真除法）；整數值在渲染與回傳 Python 時
//! 轉成整數，與舊 `int | float` 的可觀察行為一致。

use crate::ast::*;
use crate::error::RollError;
use crate::rng::DiceRng;
use crate::types::*;

// --------------------------------------------------------------------- value

/// 骰池廣播的每一「份」。
///
/// 值、渲染、優先序綁成一體——舊版是三個平行 `Vec`，長度一致只靠**慣例**。
/// 現在「三者等長」由型別保證，不可能各自漂移。
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub value: Num,
    pub render: String,
    /// 這個渲染的運算子優先序，決定它被外層包起來時要不要加括號。
    pub prec: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Scalar(Num),
    /// 裸多項骰池的廣播結果：每一份各自保留自己的數值與渲染。
    Multi(Vec<Part>),
}

impl Value {
    /// 取純量值；`Multi` 代表呼叫端用錯了。
    #[inline]
    fn scalar(&self) -> Option<Num> {
        match self {
            Value::Scalar(n) => Some(*n),
            Value::Multi(_) => None,
        }
    }
}

/// `{a, b, c}` 形式的廣播渲染。
fn join_parts(parts: &[Part]) -> String {
    let mut out = String::from("{");
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&p.render);
    }
    out.push('}');
    out
}

// --------------------------------------------------------------- fmt / math

/// Python `_fmt_num`：整數值的 float 渲染成整數。
pub fn fmt_num(v: f64) -> String {
    if v.is_nan() {
        return "nan".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    if v.fract() == 0.0 && v.abs() <= i64::MAX as f64 {
        return format!("{}", v as i64);
    }
    format!("{v}")
}

/// Python `round()` 是「四捨六入五成雙」；Rust `f64::round` 是「五離零」。
fn py_round(v: f64) -> f64 {
    let floor = v.floor();
    if v - floor == 0.5 {
        if floor.rem_euclid(2.0) == 0.0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        v.round()
    }
}

/// Python 算術：`/` 永遠產生 float；`+ - *` 任一側是 float 就是 float。
fn apply_arith(op: BinOp, a: Num, b: Num) -> Result<Num, RollError> {
    match op {
        BinOp::Add => Ok(a.arith(b, a.value + b.value)),
        BinOp::Sub => Ok(a.arith(b, a.value - b.value)),
        BinOp::Mul => Ok(a.arith(b, a.value * b.value)),
        BinOp::Div => {
            if b.value == 0.0 {
                Err(RollError::DivisionByZero)
            } else {
                Ok(Num::float(a.value / b.value))
            }
        }
    }
}

fn op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
    }
}

/// Python `math.floor` / `math.ceil` / `round` 都回傳 `int`；`abs` 保留原身分。
fn apply_func(kind: FuncKind, v: Num) -> Num {
    match kind {
        FuncKind::Floor => Num::int(v.value.floor()),
        FuncKind::Ceil => Num::int(v.value.ceil()),
        FuncKind::Round => Num::int(py_round(v.value)),
        FuncKind::Abs => Num {
            value: v.value.abs(),
            is_float: v.is_float,
        },
    }
}

fn func_name(kind: FuncKind) -> &'static str {
    match kind {
        FuncKind::Floor => "floor",
        FuncKind::Ceil => "ceil",
        FuncKind::Round => "round",
        FuncKind::Abs => "abs",
    }
}

fn prec_of(e: &Expr) -> u8 {
    match e {
        Expr::BinOp { op, .. } => op.precedence(),
        Expr::Neg(_) => 3,
        _ => 4,
    }
}

fn paren_left(s: &str, prec: u8, p: u8) -> String {
    if prec < p {
        format!("({s})")
    } else {
        s.to_string()
    }
}

fn paren_right(s: &str, prec: u8, p: u8) -> String {
    if prec <= p {
        format!("({s})")
    } else {
        s.to_string()
    }
}

/// `terms` 是否已是 `total\`(…)\`` 的自帶總計形狀。
pub fn self_contained(terms: &str, total: f64) -> bool {
    let t = fmt_num(total);
    terms.starts_with(&format!("{t}`")) && terms.ends_with('`') && terms.matches('`').count() == 2
}

// ----------------------------------------------------------------- dice ops

#[inline]
fn cp_match(value: f64, c: &CompareCondition) -> bool {
    c.op.eval(value, c.target.value)
}

/// CP 是否對 `[1, faces]` 全為真——此時重骰不可能改變任何骰子。
fn cp_always_true(c: &CompareCondition, faces: f64) -> bool {
    let t = c.target.value;
    match c.op {
        CompareOp::Ge => t <= 1.0,
        CompareOp::Gt => t < 1.0,
        CompareOp::Le => t >= faces,
        CompareOp::Lt => t > faces,
        CompareOp::Eq => t == 1.0 && faces == 1.0,
        CompareOp::Ne => false,
    }
}

fn reroll_step(d: &mut DieRoll, rng: &mut dyn DiceRng, faces: u32) {
    match &mut d.reroll_history {
        None => d.reroll_history = Some(vec![d.value]),
        Some(h) if h.last() != Some(&d.value) => h.push(d.value),
        _ => {}
    }
    d.value = rng.randint(1, faces as i64);
    if let Some(h) = d.reroll_history.as_mut() {
        h.push(d.value);
    }
    d.rerolled = true;
}

fn reroll(
    rolls: &mut [DieRoll],
    conditions: &[CompareCondition],
    recursive: bool,
    rng: &mut dyn DiceRng,
    faces: u32,
) {
    let fallback;
    let conds: &[CompareCondition] = if conditions.is_empty() {
        fallback = CompareCondition::default_reroll();
        std::slice::from_ref(&fallback)
    } else {
        conditions
    };
    let f = faces as f64;
    // 全真 CP（d1、rr>=1…）→ 重骰是 no-op，直接跳過（否則會空轉到步數上限）
    if faces >= 1 && conds.iter().all(|c| cp_always_true(c, f)) {
        return;
    }
    let matches = |v: i64| conds.iter().any(|c| cp_match(v as f64, c));

    if recursive {
        for d in rolls.iter_mut() {
            let mut steps = 0;
            while matches(d.value) && steps < MAX_REROLL_STEPS {
                reroll_step(d, rng, faces);
                steps += 1;
            }
        }
    } else {
        for d in rolls.iter_mut() {
            if matches(d.value) {
                reroll_step(d, rng, faces);
            }
        }
    }
}

fn explode(
    rolls: &mut Vec<DieRoll>,
    condition: Option<&CompareCondition>,
    rng: &mut dyn DiceRng,
    faces: u32,
    cap: Option<u32>,
) {
    let cond = condition
        .cloned()
        .unwrap_or_else(|| CompareCondition::default_explode(faces));
    let f = faces as i64;
    let mut i = 0;
    while i < rolls.len() && rolls.len() < MAX_EXPLODE_DICE {
        let (hit, depth) = {
            let d = &rolls[i];
            (cp_match(d.value as f64, &cond), d.explode_depth)
        };
        if cap.is_none_or(|c| depth < c) && hit {
            let mut child = DieRoll::new(rng.randint(1, f));
            child.exploded = true;
            child.explode_depth = depth + 1;
            rolls.push(child);
        }
        i += 1;
    }
}

fn minmax(rolls: &mut [DieRoll], kind: MinMaxKind, n: i64) {
    for d in rolls.iter_mut() {
        match kind {
            MinMaxKind::Min if d.value < n => {
                d.replaced_from = Some(d.value);
                d.value = n;
            }
            MinMaxKind::Max if d.value > n => {
                d.replaced_from = Some(d.value);
                d.value = n;
            }
            _ => {}
        }
    }
}

/// 保留/捨棄。排序鍵與舊 `_keepdrop` 完全相同：`(-value, index)` / `(value, index)`。
fn keepdrop(rolls: &mut [DieRoll], kind: KeepDropKind, n_raw: i64) {
    let active: Vec<usize> = rolls
        .iter()
        .enumerate()
        .filter(|(_, d)| !d.dropped)
        .map(|(i, _)| i)
        .collect();
    if active.is_empty() {
        return;
    }
    let n = n_raw.clamp(0, active.len() as i64) as usize;
    let mut indexed: Vec<(i64, usize)> = active.iter().map(|&i| (rolls[i].value, i)).collect();

    match kind {
        KeepDropKind::KeepHighest => {
            indexed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            for t in &indexed[n..] {
                rolls[t.1].dropped = true;
            }
        }
        KeepDropKind::KeepLowest => {
            indexed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            for t in &indexed[n..] {
                rolls[t.1].dropped = true;
            }
        }
        KeepDropKind::DropHighest => {
            indexed.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            for t in &indexed[..n] {
                rolls[t.1].dropped = true;
            }
        }
        KeepDropKind::DropLowest => {
            indexed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            for t in &indexed[..n] {
                rolls[t.1].dropped = true;
            }
        }
    }
}

/// 池層保留/捨棄：語意同骰層，但作用在「每個 term 的總值」上。
fn pool_keepdrop(values: &[Num], kept: &mut Vec<usize>, kind: KeepDropKind, n_raw: i64) {
    if kept.is_empty() {
        return;
    }
    let n = n_raw.clamp(0, kept.len() as i64) as usize;
    let mut indexed: Vec<(f64, usize)> = kept.iter().map(|&i| (values[i].value, i)).collect();

    let (drop_range, by_desc) = match kind {
        KeepDropKind::KeepHighest => (n..indexed.len(), true),
        KeepDropKind::KeepLowest => (n..indexed.len(), false),
        KeepDropKind::DropHighest => (0..n, true),
        KeepDropKind::DropLowest => (0..n, false),
    };
    if by_desc {
        indexed.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    } else {
        indexed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    }
    let dropped: Vec<usize> = indexed[drop_range].iter().map(|t| t.1).collect();
    kept.retain(|i| !dropped.contains(i));
}

/// Python `x % 2`：負數取正模，`rem_euclid` 才等價。
fn is_even(v: f64) -> bool {
    v.rem_euclid(2.0) == 0.0
}

/// 回傳 `(success 標記, 是否計入)`。
fn count_matches(kind: CountKind, v: f64, cond: Option<&CompareCondition>) -> (bool, bool) {
    match kind {
        CountKind::Even => (is_even(v), is_even(v)),
        CountKind::Odd => (!is_even(v), !is_even(v)),
        CountKind::CountSuccess => {
            let s = cond.is_none_or(|c| cp_match(v, c));
            (s, s)
        }
        CountKind::CountFailure => {
            // cs 的鏡像：計「不符 CP」的骰，success 標記仍代表「是否達成 CP」
            let s = cond.is_none_or(|c| cp_match(v, c));
            (s, !s)
        }
    }
}

fn count_sum(
    kind: CountKind,
    cond: Option<&CompareCondition>,
    rolls: &mut [DieRoll],
    active: &[usize],
) -> i64 {
    let mut total = 0i64;
    for &i in active {
        let (success, hit) = count_matches(kind, rolls[i].value as f64, cond);
        rolls[i].success = Some(success);
        if hit {
            total += 1;
        }
    }
    total
}

fn deduct_sum(
    kind: DeductKind,
    cond: Option<&CompareCondition>,
    rolls: &mut [DieRoll],
    active: &[usize],
) -> i64 {
    let mut out = 0i64;
    for &i in active {
        if cond.is_none_or(|c| cp_match(rolls[i].value as f64, c)) {
            rolls[i].success = Some(false);
            out += match kind {
                DeductKind::DeductFailures => 1,
                DeductKind::SubtractFailures => rolls[i].value,
            };
        }
    }
    out
}

/// 單顆骰的渲染（重骰鏈、min/max 取代、成功標記、刪除線）。
fn die_repr(d: &DieRoll, coin: bool) -> String {
    let mut inner = if coin {
        if d.value == 0 { "T" } else { "H" }.to_string()
    } else if let Some(h) = d.reroll_history.as_ref().filter(|h| h.len() > 1) {
        let seq = if h.last() == Some(&d.value) {
            h.clone()
        } else {
            let mut s = h.clone();
            s.push(d.value);
            s
        };
        seq.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("→")
    } else if let Some(from) = d.replaced_from {
        format!("{from}→{}", d.value)
    } else {
        d.value.to_string()
    };

    if let Some(ok) = d.success {
        inner.push(if ok { '✓' } else { '✗' });
    }
    if d.dropped {
        format!("~~{inner}~~")
    } else {
        inner
    }
}

/// 執行一組骰子。修飾詞套用順序（與舊 `_execute_dice` 一致）：
/// 重骰 → 爆炸 → min/max → 保留/捨棄 → 總和 → 成功計數 → 扣減 → 成功邊際。
fn execute_dice(node: &DiceNode, count: u32, faces: u32, rng: &mut dyn DiceRng) -> DiceGroup {
    let mut rolls: Vec<DieRoll> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let value = match node.kind {
            DiceKind::Coin { call: _ } => rng.randint(0, 1),
            DiceKind::Fate => rng.randint(-1, 1),
            DiceKind::Normal => {
                assert!(faces >= 1, "faces should be >= 1, got {}", faces);
                rng.randint(1, faces as i64)
            },
        };
        rolls.push(DieRoll::new(value));
    }

    for m in &node.mods {
        if let DiceMod::Reroll {
            recursive,
            conditions,
        } = m
        {
            reroll(&mut rolls, conditions, *recursive, rng, faces);
        }
    }
    for m in &node.mods {
        if let DiceMod::Explode { condition, cap } = m {
            explode(&mut rolls, condition.as_ref(), rng, faces, *cap);
        }
    }
    for m in &node.mods {
        if let DiceMod::MinMax { kind, bound: Some(b) } = m {
            minmax(&mut rolls, *kind, *b as i64);
        }
    }
    for m in &node.mods {
        if let DiceMod::KeepDrop { kind, n } = m {
            keepdrop(&mut rolls, *kind, *n);
        }
    }

    let active: Vec<usize> = (0..rolls.len()).filter(|&i| !rolls[i].dropped).collect();
    let mut total: i64 = match node.kind {
        DiceKind::Coin { call } => {
            let want = call as i64;
            active.iter().filter(|&&i| rolls[i].value == want).count() as i64
        }
        DiceKind::Fate => active.iter().map(|&i| rolls[i].value).sum(),
        DiceKind::Normal => active.iter().map(|&i| rolls[i].value).sum(),
    };

    for m in &node.mods {
        if let DiceMod::Count { kind, condition } = m {
            total = count_sum(*kind, condition.as_ref(), &mut rolls, &active);
        }
    }
    for m in &node.mods {
        if let DiceMod::Deduct { kind, condition } = m {
            total -= deduct_sum(*kind, condition.as_ref(), &mut rolls, &active);
        }
    }
    for m in &node.mods {
        if let DiceMod::MarginSuccess(n) = m {
            total -= *n as i64;
        }
    }

    DiceGroup {
            notation: node.notation.clone(),
            kind: node.kind.to_die_kind(),
            faces: if matches!(node.kind, DiceKind::Fate | DiceKind::Coin { .. }) { 0 } else { faces as i64 },
            rolls,
            total,
        }
}

// ------------------------------------------------------------------ evaluator

pub struct Evaluator<'r> {
    rng: &'r mut dyn DiceRng,
    groups: Vec<DiceGroup>,
}

impl<'r> Evaluator<'r> {
    pub fn new(rng: &'r mut dyn DiceRng) -> Self {
        Self {
            rng,
            groups: Vec::new(),
        }
    }

    pub fn into_groups(self) -> Vec<DiceGroup> {
        self.groups
    }

    /// 動態骰數 / 動態骰面：擲出內層（含渲染）→ 驗證為 `[lo, hi]` 內整數。
    fn eval_dynamic(
        &mut self,
        e: &Expr,
        lo: f64,
        hi: f64,
        err: RollError,
    ) -> Result<(u32, String), RollError> {
        let (value, render) = self.eval(e)?;
        let v = value
            .scalar()
            .ok_or(RollError::PoolLengthMismatch)?
            .value;
        if !(v.is_finite() && v.fract() == 0.0 && v >= lo && v <= hi) {
            return Err(err);
        }
        Ok((v as u32, render))
    }

    pub fn eval(&mut self, node: &Expr) -> Result<(Value, String), RollError> {
        match node {
            Expr::Num(v) => Ok((Value::Scalar(*v), fmt_num(v.value))),

            Expr::Neg(operand) => {
                let (value, s) = self.eval(operand)?;
                match value {
                    Value::Multi(m) => {
                        let parts: Vec<Part> = m
                            .iter()
                            .map(|p| Part {
                                value: Num {
                                    value: -p.value.value,
                                    is_float: p.value.is_float,
                                },
                                render: format!("-{}", paren_left(&p.render, p.prec, 4)),
                                prec: 3,
                            })
                            .collect();
                        let joined = join_parts(&parts);
                        Ok((Value::Multi(parts), joined))
                    }
                    Value::Scalar(v) => {
                        let s = if prec_of(operand) < 4 {
                            format!("({s})")
                        } else {
                            s
                        };
                        // Python：-int → int，-float → float
                        Ok((
                            Value::Scalar(Num {
                                value: -v.value,
                                is_float: v.is_float,
                            }),
                            format!("-{s}"),
                        ))
                    }
                }
            }

            Expr::Func { kind, arg } => {
                let (value, s) = self.eval(arg)?;
                match value {
                    Value::Multi(m) => {
                        let parts: Vec<Part> = m
                            .iter()
                            .map(|p| Part {
                                value: apply_func(*kind, p.value),
                                render: format!("{}({})", func_name(*kind), p.render),
                                prec: 4,
                            })
                            .collect();
                        let joined = join_parts(&parts);
                        Ok((Value::Multi(parts), joined))
                    }
                    Value::Scalar(v) => Ok((
                        Value::Scalar(apply_func(*kind, v)),
                        format!("{}({s})", func_name(*kind)),
                    )),
                }
            }

            Expr::Dice(node) => {
                let mut prefix = String::new();
                // 動態骰數先於骰面（inner-before-outer、left-to-right）
                let count = match &node.count {
                    DiceCount::Static(n) => *n,
                    DiceCount::Dynamic(e) => {
                        let (n, s) =
                            self.eval_dynamic(e, 1.0, 100.0, RollError::DiceCountOutOfRange)?;
                        prefix.push_str(&format!("{s} → "));
                        n
                    }
                };
                let faces = match &node.faces {
                    DiceFaces::Static(f) => *f,
                    DiceFaces::Fate => 0,
                    DiceFaces::Dynamic(e) => {
                        let (f, s) =
                            self.eval_dynamic(e, 1.0, 100_000.0, RollError::FacesOutOfRange)?;
                        prefix.push_str(&format!("{s} → "));
                        f
                    }
                };

                // 動態骰面時 `min`/`max` 的值域檢查要在這裡做（parser 不知道 faces）
                // DEBUG: print faces and kind
                let domain = match node.kind {
            DiceKind::Coin { .. } => DieKind::Coin.domain(faces),
            DiceKind::Fate => DieKind::Fate.domain(faces),
            DiceKind::Normal => DieKind::Normal.domain(faces),
        };
                for m in &node.mods {
                    if let DiceMod::MinMax { kind, bound: Some(b) } = m {
                        if let Some(e) = domain.minmax_error(*kind, *b as i64) {
                            return Err(e);
                        }
                    }
                }

                let group = execute_dice(node, count, faces, self.rng);
                let render = if group.rolls.len() > MAX_DICE_DETAIL {
                    // 明細太長：只給總計與骰組規格。
                    // 保持 `總計\`(…)\`` 的形狀，`self_contained` 才認得出來，
                    // `format_result` 就不會多補一個 `= 總計`。
                    format!(
                        "{prefix}{}`({})`",
                        fmt_num(group.total as f64),
                        group.notation
                    )
                } else {
                    let die_renders: Vec<String> = group
                        .rolls
                        .iter()
                        .map(|d| die_repr(d, matches!(node.kind, DiceKind::Coin { .. })))
                        .collect();
                    format!(
                        "{prefix}{}`({})`",
                        fmt_num(group.total as f64),
                        die_renders.join("+")
                    )
                };
                let total = group.total;
                self.groups.push(group);
                // Python：sum(骰值) / 成功計數 都是 int
                Ok((Value::Scalar(Num::int(total as f64)), render))
            }

            Expr::Pool(pool) => self.eval_pool(pool),

            Expr::BinOp { op, left, right } => {
                let (lv, ls) = self.eval(left)?;
                let (rv, rs) = self.eval(right)?;
                let p = op.precedence();
                match (lv, rv) {
                    (Value::Scalar(a), Value::Scalar(b)) => {
                        let out = apply_arith(*op, a, b)?;
                        let l = paren_left(&ls, prec_of(left), p);
                        let r = paren_right(&rs, prec_of(right), p);
                        Ok((Value::Scalar(out), format!("{l}{}{r}", op_str(*op))))
                    }
                    (Value::Multi(lm), Value::Multi(rm)) => {
                        if lm.len() != rm.len() {
                            return Err(RollError::PoolLengthMismatch);
                        }
                        let mut parts = Vec::with_capacity(lm.len());
                        for (l, r) in lm.iter().zip(rm) {
                            parts.push(Part {
                                value: apply_arith(*op, l.value, r.value)?,
                                render: format!(
                                    "{}{}{}",
                                    paren_left(&l.render, l.prec, p),
                                    op_str(*op),
                                    paren_right(&r.render, r.prec, p)
                                ),
                                prec: p,
                            });
                        }
                        let joined = join_parts(&parts);
                        Ok((Value::Multi(parts), joined))
                    }
                    (Value::Multi(lm), Value::Scalar(b)) => {
                        let r = paren_right(&rs, prec_of(right), p);
                        let mut parts = Vec::with_capacity(lm.len());
                        for l in lm {
                            parts.push(Part {
                                value: apply_arith(*op, l.value, b)?,
                                render: format!(
                                    "{}{}{r}",
                                    paren_left(&l.render, l.prec, p),
                                    op_str(*op)
                                ),
                                prec: p,
                            });
                        }
                        let joined = join_parts(&parts);
                        Ok((Value::Multi(parts), joined))
                    }
                    (Value::Scalar(a), Value::Multi(rm)) => {
                        let l = paren_left(&ls, prec_of(left), p);
                        let mut parts = Vec::with_capacity(rm.len());
                        for r in rm {
                            parts.push(Part {
                                value: apply_arith(*op, a, r.value)?,
                                render: format!(
                                    "{l}{}{}",
                                    op_str(*op),
                                    paren_right(&r.render, r.prec, p)
                                ),
                                prec: p,
                            });
                        }
                        let joined = join_parts(&parts);
                        Ok((Value::Multi(parts), joined))
                    }
                }
            }
        }
    }

    fn eval_pool(&mut self, pool: &PoolNode) -> Result<(Value, String), RollError> {
        let mut evaluated = Vec::with_capacity(pool.terms.len());
        for t in &pool.terms {
            evaluated.push(self.eval(t)?);
        }

        // 無 filter 的多項池：各 group 保留自己的總值（不靜默加總）
        if pool.mods.is_empty() && pool.terms.len() > 1 {
            let mut parts: Vec<Part> = Vec::new();
            for (term, (ev, s)) in pool.terms.iter().zip(evaluated.iter()) {
                match ev {
                    // 巢狀裸骰池：攤平（沿用舊版語意）
                    Value::Multi(m) => parts.extend(m.iter().cloned()),
                    Value::Scalar(v) => parts.push(Part {
                        value: *v,
                        render: s.clone(),
                        prec: prec_of(term),
                    }),
                }
            }
            let joined = join_parts(&parts);
            return Ok((Value::Multi(parts), joined));
        }

        // 每個 term 先化為純量（裸多項池在前面已 return）
        let values: Vec<Num> = evaluated
            .iter()
            .map(|(ev, _)| match ev {
                Value::Multi(parts) => py_sum(parts.iter().map(|p| p.value)),
                Value::Scalar(v) => *v,
            })
            .collect();
        let renders: Vec<String> = evaluated.into_iter().map(|(_, s)| s).collect();

        let mut kept: Vec<usize> = (0..values.len()).collect();
        for m in &pool.mods {
            if let PoolMod::KeepDrop { kind, n } = m {
                pool_keepdrop(&values, &mut kept, *kind, *n);
            }
        }

        let mut pool_total: Num = py_sum(kept.iter().map(|&i| values[i]));
        for m in &pool.mods {
            if let PoolMod::Count { kind, condition } = m {
                let mut sorted = kept.clone();
                sorted.sort_unstable();
                let mut hits = 0i64;
                for i in sorted {
                    let (_, hit) = count_matches(*kind, values[i].value, condition.as_ref());
                    if hit {
                        hits += 1;
                    }
                }
                pool_total = Num::int(hits as f64); // 後者覆蓋前者（與骰層一致）
            }
        }

        // 裸數字項即使被捨棄也不加刪除線
        let term_renders: Vec<String> = renders
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if kept.contains(&i) || matches!(pool.terms[i], Expr::Num(_)) {
                    r.clone()
                } else {
                    format!("~~{r}~~")
                }
            })
            .collect();

        let joined = format!("{{{}}}{}", term_renders.join(", "), pool.mods_suffix);
        Ok((Value::Scalar(pool_total), joined))
    }
}

