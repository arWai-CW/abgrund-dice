//! PyO3 綁定層：只做型別轉換，不藏任何業務邏輯。
//!
//! 對外介面與舊 `modules/RollingModule.py` 相同：
//! `roll(formula, rng=None)` / `format_result(rr)` / `split_formula_comment(src)`
//! / `RollError`。

pub mod ast;
pub mod error;
pub mod evaluator;
pub mod lexer;
pub mod parser;
pub mod rng;
pub mod engine;
pub mod types;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyAny;

use crate::rng::{CryptoRng, DiceRng};
use crate::types::{CheckResult, DiceGroup, DieRoll, Num, RollResult as CoreRollResult};

use error::PyRollError;

// ------------------------------------------------------------- num ↔ Python

/// [`Num`] → Python `int` 或 `float`，保留舊版 `int | float` 的可觀察行為。
///
/// 判斷依據是 Python 的型別身分（`/` 一定 float、`floor` 一定 int…），
/// **不是**「值是不是整數」——所以 `100/4` 回 `25.0` 而非 `25`。
fn num_to_py(py: Python<'_>, n: Num) -> PyResult<Py<PyAny>> {
    if !n.is_float && n.value.is_finite() && n.value.abs() <= i64::MAX as f64 {
        Ok((n.value as i64).into_pyobject(py)?.into_any().unbind())
    } else {
        Ok(n.value.into_pyobject(py)?.into_any().unbind())
    }
}

// ------------------------------------------------------------------- pyclass

#[pyclass(name = "DieRoll", module = "rolling_rs", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyDieRoll {
    #[pyo3(get)]
    pub value: i64,
    #[pyo3(get)]
    pub dropped: bool,
    #[pyo3(get)]
    pub rerolled: bool,
    #[pyo3(get)]
    pub exploded: bool,
    #[pyo3(get)]
    pub explode_depth: u32,
    #[pyo3(get)]
    pub success: Option<bool>,
    #[pyo3(get)]
    pub replaced_from: Option<i64>,
    #[pyo3(get)]
    pub reroll_history: Option<Vec<i64>>,
}

impl From<&DieRoll> for PyDieRoll {
    fn from(d: &DieRoll) -> Self {
        Self {
            value: d.value,
            dropped: d.dropped,
            rerolled: d.rerolled,
            exploded: d.exploded,
            explode_depth: d.explode_depth,
            success: d.success,
            replaced_from: d.replaced_from,
            reroll_history: d.reroll_history.clone(),
        }
    }
}

#[pyclass(name = "DiceGroup", module = "rolling_rs", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyDiceGroup {
    #[pyo3(get)]
    pub notation: String,
    /// `"normal"` / `"fate"` / `"coin"`。`faces == 0` 對命運骰與硬幣都有歧義，
    /// 用這個欄位判斷骰型。
    #[pyo3(get)]
    pub kind: &'static str,
    #[pyo3(get)]
    pub faces: i64,
    #[pyo3(get)]
    pub total: i64,
    rolls: Vec<DieRoll>,
}

#[pymethods]
impl PyDiceGroup {
    #[getter]
    fn rolls(&self, py: Python<'_>) -> PyResult<Vec<Py<PyDieRoll>>> {
        self.rolls
            .iter()
            .map(|d| Py::new(py, PyDieRoll::from(d)))
            .collect()
    }
}

impl From<&DiceGroup> for PyDiceGroup {
    fn from(g: &DiceGroup) -> Self {
        Self {
            notation: g.notation.clone(),
            kind: g.kind.as_str(),
            faces: g.faces,
            total: g.total,
            rolls: g.rolls.clone(),
        }
    }
}

#[pyclass(name = "CheckResult", module = "rolling_rs", frozen, skip_from_py_object)]
#[derive(Clone)]
pub struct PyCheckResult {
    #[pyo3(get)]
    pub op: String,
    #[pyo3(get)]
    pub passed: bool,
    target: Num,
}

#[pymethods]
impl PyCheckResult {
    #[getter]
    fn target(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        num_to_py(py, self.target)
    }
}

impl From<&CheckResult> for PyCheckResult {
    fn from(c: &CheckResult) -> Self {
        Self {
            op: c.op.clone(),
            passed: c.passed,
            target: c.target,
        }
    }
}

#[pyclass(name = "RollResult", module = "rolling_rs", frozen)]
pub struct PyRollResult {
    inner: CoreRollResult,
}

#[pymethods]
impl PyRollResult {
    #[getter]
    fn formula(&self) -> &str {
        &self.inner.formula
    }

    #[getter]
    fn total(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        num_to_py(py, self.inner.total)
    }

    #[getter]
    fn terms(&self) -> &str {
        &self.inner.terms
    }

    #[getter]
    fn check(&self, py: Python<'_>) -> PyResult<Option<Py<PyCheckResult>>> {
        self.inner
            .check
            .as_ref()
            .map(|c| Py::new(py, PyCheckResult::from(c)))
            .transpose()
    }

    #[getter]
    fn groups(&self, py: Python<'_>) -> PyResult<Vec<Py<PyDiceGroup>>> {
        self.inner
            .groups
            .iter()
            .map(|g| Py::new(py, PyDiceGroup::from(g)))
            .collect()
    }

    #[getter]
    fn parts(&self) -> Option<Vec<String>> {
        self.inner.parts.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "RollResult(formula={:?}, total={}, terms={:?})",
            self.inner.formula,
            self.inner.total.value,
            self.inner.terms
        )
    }
}

// ----------------------------------------------------------------- rng bridge

/// 把注入的 Python 物件（`random.Random` / `Crypto.Random`…）當 RNG 用。
///
/// `randint` 先取得一次 method 物件再逐顆呼叫，避免每顆骰子都查一次屬性。
/// `DiceRng::randint` 的簽章不能回傳錯誤，因此錯誤先快取、擲完再拋。
struct PyCallbackRng {
    method: Py<PyAny>,
    error: Option<PyErr>,
}

impl PyCallbackRng {
    /// 檢查物件有 `randint` 並預先綁定。
    fn new(obj: &Bound<'_, PyAny>) -> PyResult<Self> {
        let method = obj.getattr("randint").map_err(|_| {
            PyTypeError::new_err("rng 必須提供 randint(lo, hi)，例如 random.Random(seed)")
        })?;
        Ok(Self {
            method: method.unbind(),
            error: None,
        })
    }

    fn take_error(&mut self) -> Option<PyErr> {
        self.error.take()
    }
}

impl DiceRng for PyCallbackRng {
    fn randint(&mut self, lo: i64, hi: i64) -> i64 {
        if self.error.is_some() {
            return lo; // 已經壞了，別再呼叫 Python
        }
        Python::attach(|py| {
            match self
                .method
                .bind(py)
                .call1((lo, hi))
                .and_then(|v| v.extract::<i64>())
            {
                Ok(v) => v,
                Err(e) => {
                    self.error = Some(e);
                    lo
                }
            }
        })
    }
}

// ---------------------------------------------------------------- pyfunctions

/// 解析 + 驗證 + 擲骰。
///
/// `rng=None` 走 Rust 內建 CSPRNG（`ThreadRng`，ChaCha12 + OS 熵源）；
/// 傳入任何有 `randint(lo, hi)` 的物件（如 `random.Random(seed)`）則改用它，
/// 讓測試可以決定性注入。
#[pyfunction]
#[pyo3(signature = (formula, rng=None))]
fn roll(formula: &str, rng: Option<Bound<'_, PyAny>>) -> PyResult<PyRollResult> {
    let mut injected = match rng.as_ref() {
        Some(obj) => Some(PyCallbackRng::new(obj)?),
        None => None,
    };
    let mut native = CryptoRng::new();

    let outcome = match injected.as_mut() {
        Some(bridge) => engine::roll(formula, bridge).map_err(PyErr::from),
        None => engine::roll(formula, &mut native).map_err(PyErr::from),
    };

    // 注入的 RNG 內部錯誤優先於擲骰結果拋出
    if let Some(bridge) = injected.as_mut() {
        if let Some(e) = bridge.take_error() {
            return Err(e);
        }
    }
    Ok(PyRollResult { inner: outcome? })
}

/// 渲染成 Discord 訊息。
#[pyfunction]
fn format_result(rr: &PyRollResult) -> String {
    engine::format_result(&rr.inner)
}

/// 切成 (公式, 註解)：公式 = 最長「可成功擲骰」的前綴。
#[pyfunction]
fn split_formula_comment(src: &str) -> PyResult<(String, String)> {
    engine::split_formula_comment(src).map_err(PyErr::from)
}

/// 設定錯誤訊息語言。
///
/// 支援的值：`"zh_TW"`（預設，繁體中文）或 `"en"`（英文）。
/// 其他值會產生 `ValueError`。
#[pyfunction]
fn set_language(lang: &str) -> PyResult<()> {
    match lang {
        "zh_TW" | "zh-tw" => {
            crate::error::set_language(crate::error::Language::ZhTW);
            Ok(())
        }
        "en" | "en-US" | "en_US" => {
            crate::error::set_language(crate::error::Language::En);
            Ok(())
        }
        _ => Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(
            "Unsupported language. Use 'zh_TW' or 'en'.",
        )),
    }
}

#[pymodule]
fn abgrund_dice(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDieRoll>()?;
    m.add_class::<PyDiceGroup>()?;
    m.add_class::<PyCheckResult>()?;
    m.add_class::<PyRollResult>()?;
    m.add_function(wrap_pyfunction!(roll, m)?)?;
    m.add_function(wrap_pyfunction!(format_result, m)?)?;
    m.add_function(wrap_pyfunction!(split_formula_comment, m)?)?;
    m.add_function(wrap_pyfunction!(set_language, m)?)?;
    m.add("RollError", m.py().get_type::<PyRollError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    // 政策常數曝光給 Python：`cogs` 與測試要引用同一個數字，不要各自硬編
    m.add("MAX_DICE_DETAIL", crate::types::MAX_DICE_DETAIL)?;
    Ok(())
}
