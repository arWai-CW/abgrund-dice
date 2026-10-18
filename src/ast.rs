// rolling-rs/src/ast.rs
use crate::types::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// 純數字字面值。`Num` 帶著 Python 的 int/float 身分。
    Num(Num),
    Neg(Box<Expr>),
    BinOp {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Func {
        kind: FuncKind,
        arg: Box<Expr>,
    },
    Dice(DiceNode),
    Pool(PoolNode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn precedence(&self) -> u8 {
        match self {
            BinOp::Add | BinOp::Sub => 1,
            BinOp::Mul | BinOp::Div => 2,
        }
    }
}

/// 骰子種類（AST 層）。`Coin` 帶著「賭正面還是反面」。
///
/// 舊版只用 `faces == 0` 當 sentinel，命運骰與硬幣**無法區分**；這裡用
/// enum，`fate && coin` 這種非法組合在型別上就不存在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiceKind {
    Normal,
    Fate,
    /// `call`: 0 = tails, 1 = heads
    Coin {
        call: u8,
    },
}

impl DiceKind {
    /// 對外的骰型（不含 coin call）。
    pub const fn to_die_kind(self) -> DieKind {
        match self {
            DiceKind::Normal => DieKind::Normal,
            DiceKind::Fate => DieKind::Fate,
            DiceKind::Coin { .. } => DieKind::Coin,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiceNode {
    pub count: DiceCount,
    pub faces: DiceFaces,
    pub kind: DiceKind,
    pub mods: Vec<DiceMod>,
    pub notation: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiceCount {
    Static(u32),
    Dynamic(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiceFaces {
    Static(u32),
    Dynamic(Box<Expr>),
    Fate,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiceMod {
    KeepDrop {
        kind: KeepDropKind,
        /// 已向零截斷的 `int(n_raw)`（舊 Python 在執行期才截斷）
        n: i64,
    },
    Reroll {
        recursive: bool,
        conditions: Vec<CompareCondition>,
    },
    Explode {
        condition: Option<CompareCondition>,
        cap: Option<u32>, // None = unlimited, Some(1) = xo
    },
    MinMax {
        kind: MinMaxKind,
        bound: Option<u32>,
    },
    Count {
        kind: CountKind,
        condition: Option<CompareCondition>,
    },
    Deduct {
        kind: DeductKind,
        condition: Option<CompareCondition>,
    },
    MarginSuccess(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareCondition {
    pub op: CompareOp,
    pub target: Num,
}

impl CompareCondition {
    pub fn default_reroll() -> Self {
        Self {
            op: CompareOp::Eq,
            target: Num::int(1.0),
        }
    }

    pub fn default_explode(faces: u32) -> Self {
        Self {
            op: CompareOp::Eq,
            target: Num::int(faces as f64),
        }
    }

    pub fn matches(&self, value: f64) -> bool {
        self.op.eval(value, self.target.value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolNode {
    pub terms: Vec<Expr>,
    pub mods: Vec<PoolMod>,
    pub mods_suffix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PoolMod {
    KeepDrop {
        kind: KeepDropKind,
        /// 已向零截斷的 `int(n_raw)`
        n: i64,
    },
    Count {
        kind: CountKind,
        condition: Option<CompareCondition>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRoll {
    pub expr: Expr,
    pub check: Option<CompareCondition>,
}
