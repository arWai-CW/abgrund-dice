use crate::error::RollError;


/// Python 的 `int | float`。
///
/// `is_float` **只**影響回傳給 Python 的型別（`isinstance(total, int)`、`repr()`）；
/// 所有顯示都走 [`crate::evaluator::fmt_num`]，把整數值的 float 印成整數，
/// 因此渲染結果完全不受這個旗標影響。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Num {
    pub value: f64,
    pub is_float: bool,
}

impl Num {
    #[inline]
    pub const fn int(value: f64) -> Self {
        Self {
            value,
            is_float: false,
        }
    }

    #[inline]
    pub const fn float(value: f64) -> Self {
        Self {
            value,
            is_float: true,
        }
    }

    /// Python 字面值規則：`\d+` → `int`，`\d+.\d+` → `float`。
    #[inline]
    pub fn literal(text: &str, value: f64) -> Self {
        Self {
            value,
            is_float: text.contains('.'),
        }
    }

    /// Python `+ - *`：任一側是 float，結果就是 float。
    #[inline]
    pub const fn arith(self, other: Self, value: f64) -> Self {
        Self {
            value,
            is_float: self.is_float || other.is_float,
        }
    }

    #[inline]
    pub const fn add(self, other: Self) -> Self {
        self.arith(other, self.value + other.value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DieRoll {
    pub value: i64,
    pub dropped: bool,
    pub rerolled: bool,
    pub exploded: bool,
    pub explode_depth: u32,
    pub success: Option<bool>,
    pub replaced_from: Option<i64>,
    pub reroll_history: Option<Vec<i64>>,
}

impl DieRoll {
    pub fn new(value: i64) -> Self {
        Self {
            value,
            dropped: false,
            rerolled: false,
            exploded: false,
            explode_depth: 0,
            success: None,
            replaced_from: None,
            reroll_history: None,
        }
    }

}

/// 骰子種類。
///
/// 舊版只用 `faces == 0` 當 sentinel，**命運骰與硬幣無法區分**——消費端
/// （例如 `cogs/fateDiceRoll.py`）只能靠「公式是我自己組的」來假設。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DieKind {
    /// 普通骰：值域 `[1, faces]`。
    Normal,
    /// 命運骰（`dF`）：值域 `[-1, 1]`，`faces` 固定 0。
    Fate,
    /// 硬幣（`dc`）：值域 `[0, 1]`（0 = T、1 = H），`faces` 固定 0。
    Coin,
}

impl DieKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            DieKind::Normal => "normal",
            DieKind::Fate => "fate",
            DieKind::Coin => "coin",
        }
    }

    /// 這組骰子的合法值域。
    #[inline]
    pub const fn domain(self, faces: u32) -> DiceDomain {
        match self {
            DieKind::Normal => DiceDomain::normal(faces),
            DieKind::Fate => DiceDomain {
                lo: -1,
                hi: 1,
            },
            DieKind::Coin => DiceDomain::coin(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiceGroup {
    pub notation: String,
    /// 骰子種類。舊版沒有這個欄位，只能從 `faces == 0` 猜（有歧義）。
    pub kind: DieKind,
    /// 骰面數；`Fate` / `Coin` 固定為 0。
    pub faces: i64,
    pub rolls: Vec<DieRoll>,
    pub total: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    pub op: String,
    pub target: Num,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RollResult {
    pub formula: String,
    /// Python 的 `int | float` 身分（見 [`Num`]）。
    pub total: Num,
    pub terms: String,
    pub check: Option<CheckResult>,
    pub groups: Vec<DiceGroup>,
    pub parts: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl CompareOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "=" => Some(Self::Eq),
            ">=" => Some(Self::Ge),
            ">" => Some(Self::Gt),
            "<=" => Some(Self::Le),
            "<" => Some(Self::Lt),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ge => ">=",
            Self::Gt => ">",
            Self::Le => "<=",
            Self::Lt => "<",
            Self::Ne => "!=",
        }
    }

    pub fn eval(&self, left: f64, right: f64) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Gt => left > right,
            Self::Ge => left >= right,
            Self::Lt => left < right,
            Self::Le => left <= right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepDropKind {
    KeepHighest,
    KeepLowest,
    DropHighest,
    DropLowest,
}

impl KeepDropKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kh" | "k" => Some(Self::KeepHighest),
            "kl" => Some(Self::KeepLowest),
            "dh" => Some(Self::DropHighest),
            "dl" | "d" => Some(Self::DropLowest),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKind {
    CountSuccess,
    CountFailure,
    Even,
    Odd,
}

impl CountKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cs" => Some(Self::CountSuccess),
            "cf" => Some(Self::CountFailure),
            "even" => Some(Self::Even),
            "odd" => Some(Self::Odd),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeductKind {
    DeductFailures,
    SubtractFailures,
}

impl DeductKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "df" => Some(Self::DeductFailures),
            "sf" => Some(Self::SubtractFailures),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinMaxKind {
    Min,
    Max,
}

impl MinMaxKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuncKind {
    Floor,
    Ceil,
    Round,
    Abs,
}

impl FuncKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "floor" => Some(Self::Floor),
            "ceil" => Some(Self::Ceil),
            "round" => Some(Self::Round),
            "abs" => Some(Self::Abs),
            _ => None,
        }
    }
}

/// Python `sum(iterable)`：累加器從 `0`（int）開始，碰到 float 就變 float。
pub fn py_sum(nums: impl IntoIterator<Item = Num>) -> Num {
    nums.into_iter().fold(Num::int(0.0), Num::add)
}

/// 一顆（一組）骰子的合法值域。
///
/// `min N` 把低於 N 的值抬到 N；`max N` 把高於 N 的值壓到 N。
/// 只有**會把值推出這個範圍**的參數才算錯——無害的 no-op（`4d6max8`）
/// 必須繼續放行，否則長公式會動不動被誤擋。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiceDomain {
    pub lo: i64,
    pub hi: i64,
}

impl DiceDomain {
    /// 普通骰：`[1, faces]`。
    #[inline]
    pub const fn normal(faces: u32) -> Self {
        Self {
            lo: 1,
            hi: faces as i64,
        }
    }

    /// 硬幣：`[0, 1]`（0 = T、1 = H）。
    #[inline]
    pub const fn coin() -> Self {
        Self { lo: 0, hi: 1 }
    }

    /// 這個 `min` / `max` 參數會不會把骰值推出值域？回傳 `Some(err)` 表示違規。
    ///
    /// 刻意**不對稱**：`min0`、`max100` 都是無害 no-op（不改變任何值），
    /// 只有真的會把值推出去的方向才擋。
    #[inline]
    pub fn minmax_error(self, kind: MinMaxKind, bound: i64) -> Option<RollError> {
        match kind {
            MinMaxKind::Min if bound > self.hi => Some(RollError::MinTooLarge(self.hi)),
            MinMaxKind::Max if bound < self.lo => Some(RollError::MaxTooSmall(self.lo)),
            _ => None,
        }
    }
}

pub const MAX_EXPLODE_DICE: usize = 1000;

/// 超過這個骰數就不列明細，只給「總計 + 骰組規格」（``349`(21d100)` ``）。
///
/// 理由：明細是訊息長度的主要來源。100 顆骰約 300 字元，
/// `,r 50 {100d6 ×8}` 因此產生 85 KB 的訊息（Discord 單則上限 2000）。
/// 超過這個數量沒人會逐顆看，但長度會爆掉。
///
/// 門檻比的是**實際擲出的骰數**，不是宣告的骰數——`4d6x<=6` 宣告 4 顆卻
/// 會爆炸出 1000 顆，用宣告值判斷就擋不住。
pub const MAX_DICE_DETAIL: usize = 20;
pub const MAX_REROLL_STEPS: usize = 1000;
pub const MAX_PARSE_DEPTH: usize = 100;
pub const MAX_FORMULA_LEN: usize = 512;