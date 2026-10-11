"""FROZEN ORACLE — 移植前的純 Python 實作，只給差分測試用，bot 不 import。

出處：git commit c1ffc07 `modules/RollingModule.py`。
用途：`tests/test_rolling_differential.py` 拿它與 Rust 實作對拍，
      確保 PyO3 改寫沒有改變任何可觀察行為。**不要修改此檔**——
      它是 oracle，改了就不再是 oracle。
"""

from __future__ import annotations

import math
import random
import re
from dataclasses import dataclass

__all__ = [
    "DieRoll",
    "DiceGroup",
    "CheckResult",
    "RollResult",
    "RollError",
    "roll",
    "format_result",
    "split_formula_comment",
]

_DEFAULT_RNG = random.Random()

# ponytail: runaway caps — `1d1x` / `rr>=1` would otherwise spin forever;
# raise if a legitimate table ever needs longer chains.
_MAX_EXPLODE_DICE = 1000
_MAX_REROLL_STEPS = 1000
# parser nesting cap — ~4 frames/level × 100 stays under CPython's 1000 limit
_MAX_PARSE_DEPTH = 100

_CHECK_OPS = (">=", ">", "<=", "<")
_MOD_IDS = frozenset(
    {
        "kh",
        "k",
        "kl",
        "dh",
        "dl",
        "d",
        "r",
        "rr",
        "x",
        "xo",
        "min",
        "max",
        "cs",
        "cf",
        "df",
        "sf",
        "ms",
        "even",
        "odd",
    }
)
_COUNT_MOD_IDS = frozenset({"cs", "cf", "even", "odd"})
_POOL_MOD_IDS = frozenset({"kh", "k", "kl", "dh", "dl", "d", "cs", "cf", "even", "odd"})
_DEDUCT_MOD_IDS = frozenset({"df", "sf"})
_FUNC_IDS = frozenset({"floor", "ceil", "round", "abs"})
_PREC = {"+": 1, "-": 1, "*": 2, "/": 2}


class RollError(Exception):
    """可讀骰子錯誤；訊息一律含「錯誤」。"""


@dataclass(slots=True)
class DieRoll:
    value: int
    dropped: bool = False
    rerolled: bool = False
    exploded: bool = False
    explode_depth: int = 0
    success: bool | None = None
    replaced_from: int | None = None
    reroll_history: list[int] | None = None  # first roll → … → final (r/rr)


@dataclass(slots=True)
class DiceGroup:
    notation: str
    faces: int  # fate groups use 0 sentinel (values are -1/0/1)
    rolls: list[DieRoll]
    total: int


@dataclass(slots=True)
class CheckResult:
    op: str
    target: float | int
    passed: bool


@dataclass(slots=True)
class RollResult:
    formula: str
    total: float | int
    terms: str
    check: CheckResult | None
    groups: list[DiceGroup]
    # bare multi-group pool (no `}`-filter): per-group renders for `#1.1` split;
    # None → scalar result (normal `= total` path)
    parts: list[str] | None = None


# --- tokenizer ---


_TOKEN_RE = re.compile(
    r"""
    (?P<ws>\s+)
  | (?P<num>\d+\.\d+|\d+)
  | (?P<ident>[A-Za-z]+)
  | (?P<cmp>>=|<=|>|<|=)
  | (?P<op>[+\-*/(){},])
""",
    re.VERBOSE,
)


@dataclass(slots=True)
class _Tok:
    kind: str  # num | ident | cmp | op
    val: object
    start: int
    end: int


def _tokenize(src: str) -> list[_Tok]:
    toks: list[_Tok] = []
    i = 0
    while i < len(src):
        m = _TOKEN_RE.match(src, i)
        if m is None:
            raise RollError(f"錯誤：無法解析的公式：{src}")
        i = m.end()
        kind = m.lastgroup
        if kind == "ws":
            continue
        if kind == "num":
            text = m.group()
            val: object = float(text) if "." in text else int(text)
            toks.append(_Tok("num", val, m.start(), m.end()))
        elif kind == "ident":
            toks.append(_Tok("ident", m.group(), m.start(), m.end()))
        elif kind == "cmp":
            toks.append(_Tok("cmp", m.group(), m.start(), m.end()))
        else:
            toks.append(_Tok("op", m.group(), m.start(), m.end()))
    return toks


# --- AST ---


class _Num:
    __slots__ = ("value",)

    def __init__(self, value: float | int) -> None:
        self.value = value


class _Neg:
    __slots__ = ("operand",)

    def __init__(self, operand: object) -> None:
        self.operand = operand


class _Bin:
    __slots__ = ("op", "left", "right")

    def __init__(self, op: str, left: object, right: object) -> None:
        self.op = op
        self.left = left
        self.right = right


class _Func:
    __slots__ = ("name", "arg")

    def __init__(self, name: str, arg: object) -> None:
        self.name = name
        self.arg = arg


class _KdMod:
    __slots__ = ("name", "n")

    def __init__(self, name: str, n: float | int) -> None:
        self.name = name  # kh|k|kl|dh|dl|d
        self.n = n


class _RMod:
    __slots__ = ("recursive", "cps")

    def __init__(self, recursive: bool, cps: list[tuple[str, float | int]]) -> None:
        self.recursive = recursive
        self.cps = cps  # empty → default ("=", 1) at execution


class _XMod:
    __slots__ = ("cp", "cap")

    def __init__(
        self, cp: tuple[str, float | int] | None, cap: int | None = None
    ) -> None:
        self.cp = cp  # None → default ("=", faces) at execution
        self.cap = cap  # None → unlimited; xo → 1


class _MinMaxMod:
    __slots__ = ("kind", "n")

    def __init__(self, kind: str, n: float | int | None) -> None:
        self.kind = kind  # min|max
        self.n = n  # None → no replacement


class _CondMod:
    __slots__ = ("kind", "cp")

    def __init__(
        self, kind: str, cp: tuple[str, float | int] | None
    ) -> None:
        self.kind = kind  # cs|cf|df|sf|even|odd
        self.cp = cp  # None → every die matches


class _MsMod:
    __slots__ = ("n",)

    def __init__(self, n: float | int) -> None:
        self.n = n


class _Pool:
    __slots__ = ("terms", "mods", "mods_suffix")

    def __init__(self, terms: list[object], mods: list[object], mods_suffix: str) -> None:
        self.terms = terms
        self.mods = mods
        self.mods_suffix = mods_suffix  # raw source after `}` (e.g. "kh", "cs>15")


class _MultiValue:
    """Bare multi-group pool result — each group keeps its own total (no reduce)."""

    __slots__ = ("values", "renders", "precs")

    def __init__(
        self,
        values: list[float | int],
        renders: list[str],
        precs: list[int],
    ) -> None:
        self.values = values
        self.renders = renders
        self.precs = precs


class _Dice:
    __slots__ = (
        "n",
        "faces",
        "fate",
        "mods",
        "notation",
        "group",
        "coin",
        "call",
        "dyn_prefix",
    )

    def __init__(
        self,
        n: object,
        faces: object,
        fate: bool,
        mods: list[object],
        notation: str,
        *,
        coin: bool = False,
        call: int = 1,
    ) -> None:
        self.n = n
        self.faces = faces
        self.fate = fate
        self.mods = mods
        self.notation = notation
        self.group: DiceGroup | None = None
        self.coin = coin
        self.call = call  # coin: 1=H tally, 0=T tally
        self.dyn_prefix = ""  # dynamic n/faces render prefix, e.g. "17`(17)` → "


def _is_int_in(v: object, lo: int, hi: int) -> bool:
    return isinstance(v, (int, float)) and v == int(v) and lo <= int(v) <= hi


def _validate_dice(n: object, faces: object, fate: bool) -> None:
    # dynamic (AST node) values skip parse-time validation — checked at execution
    if isinstance(n, (int, float)) and not _is_int_in(n, 1, 100):
        raise RollError("錯誤：骰數必須介於 1~100")
    if fate:
        return
    if isinstance(faces, (int, float)) and not _is_int_in(faces, 1, 100000):
        raise RollError("錯誤：骰面必須介於 1~100000")


# --- parser ---


class _Parser:
    def __init__(self, toks: list[_Tok], src: str) -> None:
        self.toks = toks
        self.src = src
        self.i = 0
        self.last_end = 0
        self.depth = 0

    def _enter(self) -> None:
        """Guard recursive descent (``(...)`` / ``{...}``) against stack overflow."""
        if self.depth >= _MAX_PARSE_DEPTH:
            raise self._fail()
        self.depth += 1

    def peek(self) -> _Tok | None:
        return self.toks[self.i] if self.i < len(self.toks) else None

    def next(self) -> _Tok:
        t = self.toks[self.i]
        self.i += 1
        self.last_end = t.end
        return t

    def _fail(self) -> RollError:
        return RollError(f"錯誤：無法解析的公式：{self.src}")

    def parse_roll(self) -> tuple[object, tuple[str, float | int] | None]:
        expr = self.parse_expr()
        check: tuple[str, float | int] | None = None
        t = self.peek()
        if t is not None and t.kind == "cmp" and t.val in _CHECK_OPS:
            self.next()
            nt = self.peek()
            if nt is None or nt.kind != "num":
                raise self._fail()
            self.next()
            check = (t.val, nt.val)
        if self.peek() is not None:
            raise self._fail()
        return expr, check

    def parse_expr(self) -> object:
        node = self.parse_term()
        while True:
            t = self.peek()
            if t is None or t.kind != "op" or t.val not in ("+", "-"):
                return node
            self.next()
            node = _Bin(t.val, node, self.parse_term())

    def parse_term(self) -> object:
        node = self.parse_factor()
        while True:
            t = self.peek()
            if t is None or t.kind != "op" or t.val not in ("*", "/"):
                return node
            self.next()
            node = _Bin(t.val, node, self.parse_factor())

    def parse_factor(self) -> object:
        t = self.peek()
        if t is not None and t.kind == "op" and t.val == "-":
            self.next()
            return _Neg(self.parse_atom())
        return self.parse_atom()

    def parse_atom(self) -> object:
        t = self.peek()
        if t is None:
            raise self._fail()
        if t.kind == "num":
            self.next()
            nxt = self.peek()
            if nxt is not None and nxt.kind == "ident":
                nxt_name = nxt.val
                if nxt_name in ("d", "df", "dF"):
                    self.next()
                    if nxt_name == "d":
                        return self._parse_dice_body(
                            t.val, t.start, fate_marker=False
                        )
                    return self._parse_dice_body(t.val, t.start, fate_marker=True)
                if str(nxt_name).startswith("dc"):
                    self.next()
                    return self._parse_coin_body(t.val, t.start, str(nxt_name))
            return _Num(t.val)
        if t.kind == "ident":
            name = t.val
            if name in _FUNC_IDS:
                self.next()
                lp = self.peek()
                if lp is None or lp.kind != "op" or lp.val != "(":
                    raise self._fail()
                self.next()
                arg = self.parse_expr()
                rp = self.peek()
                if rp is None or rp.kind != "op" or rp.val != ")":
                    raise self._fail()
                self.next()
                return _Func(name, arg)
            if name == "d":
                self.next()
                return self._parse_dice_body(1, t.start, fate_marker=False)
            if name in ("df", "dF"):
                self.next()
                return self._parse_dice_body(1, t.start, fate_marker=True)
            if name.startswith("dc"):
                self.next()
                return self._parse_coin_body(1, t.start, name)
            raise self._fail()
        if t.kind == "op" and t.val == "(":
            start = t.start
            self.next()
            self._enter()
            try:
                e = self.parse_expr()
            finally:
                self.depth -= 1
            rp = self.peek()
            if rp is None or rp.kind != "op" or rp.val != ")":
                raise self._fail()
            self.next()
            nxt = self.peek()
            if nxt is not None and nxt.kind == "ident":
                nxt_name = str(nxt.val)
                if nxt_name == "d":
                    self.next()
                    return self._parse_dice_body(e, start, fate_marker=False)
                if nxt_name in ("df", "dF"):
                    self.next()
                    return self._parse_dice_body(e, start, fate_marker=True)
                if nxt_name.startswith("dc"):
                    self.next()
                    return self._parse_coin_body(e, start, nxt_name)
            return e  # plain parenthetical
        if t.kind == "op" and t.val == "{":
            self.next()
            self._enter()
            try:
                terms: list[object] = []
                while True:
                    terms.append(self.parse_expr())
                    nt = self.peek()
                    if nt is None or nt.kind != "op":
                        raise self._fail()
                    if nt.val == ",":
                        self.next()
                        continue
                    if nt.val == "}":
                        self.next()
                        break
                    raise self._fail()
            finally:
                self.depth -= 1
            brace_end = self.last_end
            mods = self._parse_pool_mods()
            mods_suffix = self.src[brace_end : self.last_end]
            return _Pool(terms, mods, mods_suffix)
        raise self._fail()

    def _parse_dice_body(
        self, n: object, start: int, *, fate_marker: bool
    ) -> _Dice:
        fate = fate_marker
        faces: object = 0
        if not fate_marker:
            t = self.peek()
            if t is not None and t.kind == "ident" and t.val in ("f", "F"):
                self.next()
                fate = True
            else:
                sign = 1
                if t is not None and t.kind == "op" and t.val == "-":
                    self.next()
                    sign = -1
                    t = self.peek()
                if t is not None and t.kind == "op" and t.val == "(":
                    # parenthetical faces — dynamic, resolved in _roll_tree
                    self.next()
                    inner = self.parse_expr()
                    rp = self.peek()
                    if rp is None or rp.kind != "op" or rp.val != ")":
                        raise self._fail()
                    self.next()
                    faces = _Neg(inner) if sign == -1 else inner
                else:
                    if t is None or t.kind != "num":
                        raise self._fail()
                    self.next()
                    faces = sign * t.val
        _validate_dice(n, faces, fate)
        # grammar: fate production has no <mods>
        mods: list[object] = [] if fate else self.parse_mods()
        notation = self.src[start : self.last_end]
        return _Dice(n, faces, fate, mods, notation)

    def _parse_pool_mods(self) -> list[object]:
        """Pool-level mods after `}`: (kh|k|kl|dh|dl|d)[num] | (cs|cf|even|odd)[cp].

        Anything else stops the loop; leftover tokens make ``parse_roll`` fail.
        """
        mods: list[object] = []
        while True:
            t = self.peek()
            if t is None or t.kind != "ident" or t.val not in _POOL_MOD_IDS:
                return mods
            name = t.val
            if name in ("kh", "k", "kl", "dh", "dl", "d"):
                self.next()
                n: float | int = 1
                nt = self.peek()
                if nt is not None and nt.kind == "num":
                    self.next()
                    n = nt.val
                mods.append(_KdMod(name, n))
            elif name in ("even", "odd"):
                self.next()
                mods.append(_CondMod(name, None))
            else:  # cs | cf
                self.next()
                cp = self._parse_cp() if self._cp_ahead() else None
                mods.append(_CondMod(name, cp))

    def _parse_coin_body(self, n: object, start: int, ident_val: str) -> _Dice:
        if ident_val == "dc":
            call = 1
        elif ident_val == "dcc":
            nt = self.peek()
            if nt is None or nt.kind != "num" or nt.val not in (0, 1):
                raise self._fail()
            self.next()
            call = int(nt.val)
        else:
            # e.g. `4dcx` — coin token with attached modifier letters
            raise RollError("錯誤：硬幣不支援該修飾詞")
        _validate_dice(n, 0, True)
        mods = self.parse_mods()
        for m in mods:
            if isinstance(m, (_KdMod, _RMod, _XMod)):
                raise RollError("錯誤：硬幣不支援該修飾詞")
        notation = self.src[start : self.last_end]
        return _Dice(n, 0, False, mods, notation, coin=True, call=call)

    def parse_mods(self) -> list[object]:
        mods: list[object] = []
        while True:
            t = self.peek()
            if t is None or t.kind != "ident" or t.val not in _MOD_IDS:
                return mods
            name = t.val
            if name in ("kh", "k", "kl", "dh", "dl", "d"):
                self.next()
                n: float | int = 1
                nt = self.peek()
                if nt is not None and nt.kind == "num":
                    self.next()
                    n = nt.val
                mods.append(_KdMod(name, n))
            elif name in ("r", "rr"):
                self.next()
                cps: list[tuple[str, float | int]] = []
                while self._cp_ahead():
                    cps.append(self._parse_cp())
                mods.append(_RMod(name == "rr", cps))
            elif name in ("x", "xo"):
                self.next()
                cap: int | None = None
                if name == "xo":
                    cap = 1
                else:
                    nt = self.peek()
                    if nt is not None and nt.kind == "num":
                        self.next()
                        if not _is_int_in(nt.val, 0, _MAX_EXPLODE_DICE):
                            raise RollError(
                                f"錯誤：爆炸次數上限為 {_MAX_EXPLODE_DICE}"
                            )
                        cap = int(nt.val)
                cp = self._parse_cp() if self._cp_ahead() else None
                mods.append(_XMod(cp, cap))
            elif name in ("min", "max"):
                self.next()
                bound: float | int | None = None
                nt = self.peek()
                if nt is not None and nt.kind == "num":
                    self.next()
                    if not _is_int_in(nt.val, 0, 100000):
                        raise RollError("錯誤：數值必須介於 0~100000")
                    bound = int(nt.val)
                mods.append(_MinMaxMod(name, bound))
            elif name == "ms":
                self.next()
                nt = self.peek()
                if nt is None or nt.kind != "num":
                    raise self._fail()
                self.next()
                if not _is_int_in(nt.val, 0, 100000):
                    raise RollError("錯誤：數值必須介於 0~100000")
                mods.append(_MsMod(int(nt.val)))
            elif name in ("even", "odd"):
                self.next()
                mods.append(_CondMod(name, None))
            else:  # cs | cf | df | sf
                self.next()
                cp = self._parse_cp() if self._cp_ahead() else None
                mods.append(_CondMod(name, cp))

    def _cp_ahead(self) -> bool:
        t = self.peek()
        return t is not None and t.kind in ("cmp", "num")

    def _parse_cp(self) -> tuple[str, float | int]:
        t = self.next()
        if t.kind == "cmp":
            nt = self.peek()
            if nt is None or nt.kind != "num":
                raise self._fail()
            self.next()
            return (t.val, nt.val)
        return ("=", t.val)


# --- execution ---


def _cp_match(value: float | int, op: str, target: float | int) -> bool:
    if op == ">":
        return value > target
    if op == ">=":
        return value >= target
    if op == "<":
        return value < target
    if op == "<=":
        return value <= target
    return value == target  # "=" / default


def _cp_always_true(op: str, target: float | int, faces: int) -> bool:
    """True when the CP matches every value in ``[1, faces]`` (reroll is a no-op)."""
    if op == ">=":
        return target <= 1
    if op == ">":
        return target < 1
    if op == "<=":
        return target >= faces
    if op == "<":
        return target > faces
    return target == 1 and faces == 1  # "="


def _reroll(
    rolls: list[DieRoll],
    cps: list[tuple[str, float | int]],
    recursive: bool,
    rng: random.Random,
    faces: float | int,
) -> None:
    conds = cps or [("=", 1)]
    f = int(faces)
    # always-true CPs over [1, f] (d1=1, rr>=1, …): reroll cannot change any
    # die — skip outright (distribution-preserving for uniform dice).
    if f >= 1 and all(_cp_always_true(op, t, f) for op, t in conds):
        return

    def matches(v: float | int) -> bool:
        return any(_cp_match(v, op, t) for op, t in conds)

    def step(d: DieRoll) -> None:
        if d.reroll_history is None:
            d.reroll_history = [d.value]
        elif d.reroll_history[-1] != d.value:
            d.reroll_history.append(d.value)  # prior r-mod left value unlogged
        d.value = rng.randint(1, f)
        d.reroll_history.append(d.value)
        d.rerolled = True

    if recursive:
        for d in rolls:
            steps = 0
            while matches(d.value) and steps < _MAX_REROLL_STEPS:
                step(d)
                steps += 1
    else:
        for d in rolls:
            if matches(d.value):
                step(d)


def _explode(
    rolls: list[DieRoll],
    cp: tuple[str, float | int] | None,
    rng: random.Random,
    faces: float | int,
    cap: int | None = None,
) -> None:
    op, target = cp if cp is not None else ("=", int(faces))
    f = int(faces)
    i = 0
    while i < len(rolls) and len(rolls) < _MAX_EXPLODE_DICE:
        d = rolls[i]
        if (cap is None or d.explode_depth < cap) and _cp_match(
            d.value, op, target
        ):
            rolls.append(
                DieRoll(
                    rng.randint(1, f),
                    exploded=True,
                    explode_depth=d.explode_depth + 1,
                )
            )
        i += 1


def _minmax(rolls: list[DieRoll], kind: str, n: int) -> None:
    for d in rolls:
        if kind == "min" and d.value < n:
            d.replaced_from = d.value
            d.value = n
        elif kind == "max" and d.value > n:
            d.replaced_from = d.value
            d.value = n


def _cond_ok(d: DieRoll, cp: tuple[str, float | int] | None) -> bool:
    if cp is None:
        return True
    return _cp_match(d.value, cp[0], cp[1])


def _count(mod: _CondMod, active: list[DieRoll]) -> int:
    total = 0
    for d in active:
        if mod.kind == "even":
            success = d.value % 2 == 0
            hit = success
        elif mod.kind == "odd":
            success = d.value % 2 == 1
            hit = success
        elif mod.kind == "cf":
            # plan §2.2: cf tallies dice that do NOT meet the CP
            success = _cond_ok(d, mod.cp)
            hit = not success
        else:  # cs
            success = _cond_ok(d, mod.cp)
            hit = success
        d.success = success
        if hit:
            total += 1
    return total


def _deduct(mod: _CondMod, active: list[DieRoll]) -> int:
    hits = [d for d in active if _cond_ok(d, mod.cp)]
    for d in hits:
        d.success = False
    if mod.kind == "df":
        return len(hits)
    return sum(d.value for d in hits)  # sf


def _keepdrop(rolls: list[DieRoll], name: str, n_raw: float | int) -> None:
    active = [i for i, d in enumerate(rolls) if not d.dropped]
    if not active:
        return
    n = int(n_raw)
    n = max(0, min(n, len(active)))
    indexed = [(rolls[i].value, i) for i in active]

    if name in ("kh", "k"):
        selected = {i for _, i in sorted(indexed, key=lambda t: (-t[0], t[1]))[:n]}
        for i in active:
            if i not in selected:
                rolls[i].dropped = True
    elif name == "kl":
        selected = {i for _, i in sorted(indexed, key=lambda t: (t[0], t[1]))[:n]}
        for i in active:
            if i not in selected:
                rolls[i].dropped = True
    elif name == "dh":
        for _, i in sorted(indexed, key=lambda t: (-t[0], t[1]))[:n]:
            rolls[i].dropped = True
    else:  # dl / d
        for _, i in sorted(indexed, key=lambda t: (t[0], t[1]))[:n]:
            rolls[i].dropped = True


def _pool_keepdrop(
    values: list[float | int], kept: set[int], name: str, n_raw: float | int
) -> None:
    """Pool-term keep/drop — same selection semantics as dice-layer ``_keepdrop``."""
    active = sorted(kept)
    if not active:
        return
    n = int(n_raw)
    n = max(0, min(n, len(active)))
    indexed = [(values[i], i) for i in active]

    if name in ("kh", "k"):
        selected = {i for _, i in sorted(indexed, key=lambda t: (-t[0], t[1]))[:n]}
        kept.intersection_update(selected)
    elif name == "kl":
        selected = {i for _, i in sorted(indexed, key=lambda t: (t[0], t[1]))[:n]}
        kept.intersection_update(selected)
    elif name == "dh":
        for _, i in sorted(indexed, key=lambda t: (-t[0], t[1]))[:n]:
            kept.discard(i)
    else:  # dl / d
        for _, i in sorted(indexed, key=lambda t: (t[0], t[1]))[:n]:
            kept.discard(i)


def _execute_dice(node: _Dice, rng: random.Random) -> DiceGroup:
    n = int(node.n)
    rolls: list[DieRoll] = []
    if node.coin:
        for _ in range(n):
            rolls.append(DieRoll(rng.randint(0, 1)))
    elif node.fate:
        for _ in range(n):
            rolls.append(DieRoll(rng.randint(-1, 1)))
    else:
        f = int(node.faces)
        for _ in range(n):
            rolls.append(DieRoll(rng.randint(1, f)))

    # plan §2.4: reroll → explode → min/max → keep/drop → count → deduct → ms
    for m in node.mods:
        if isinstance(m, _RMod):
            _reroll(rolls, m.cps, m.recursive, rng, node.faces)
    for m in node.mods:
        if isinstance(m, _XMod):
            _explode(rolls, m.cp, rng, node.faces, m.cap)
    for m in node.mods:
        if isinstance(m, _MinMaxMod) and m.n is not None:
            _minmax(rolls, m.kind, int(m.n))
    for m in node.mods:
        if isinstance(m, _KdMod):
            _keepdrop(rolls, m.name, m.n)

    active = [d for d in rolls if not d.dropped]
    if node.coin:
        total: float | int = sum(1 for d in active if d.value == node.call)
    else:
        total = sum(d.value for d in active)
    for m in node.mods:
        if isinstance(m, _CondMod) and m.kind in _COUNT_MOD_IDS:
            total = _count(m, active)
    for m in node.mods:
        if isinstance(m, _CondMod) and m.kind in _DEDUCT_MOD_IDS:
            total -= _deduct(m, active)
    for m in node.mods:
        if isinstance(m, _MsMod):
            total -= m.n

    return DiceGroup(
        notation=node.notation,
        faces=0 if node.fate else int(node.faces),
        rolls=rolls,
        total=total,
    )


def _roll_tree(node: object, rng: random.Random, groups: list[DiceGroup]) -> None:
    if isinstance(node, _Dice):
        # dynamic n first, then faces (left-to-right, inner-before-outer)
        if not isinstance(node.n, (int, float)):
            _roll_tree(node.n, rng, groups)
            v, s = _eval(node.n)
            if not _is_int_in(v, 1, 100):
                raise RollError("錯誤：骰數必須介於 1~100")
            node.dyn_prefix += f"{s} → "
            node.n = int(v)
        if not isinstance(node.faces, (int, float)):
            _roll_tree(node.faces, rng, groups)
            v, s = _eval(node.faces)
            if not _is_int_in(v, 1, 100000):
                raise RollError("錯誤：骰面必須介於 1~100000")
            node.dyn_prefix += f"{s} → "
            node.faces = int(v)
        node.group = _execute_dice(node, rng)
        groups.append(node.group)
    elif isinstance(node, _Pool):
        for term in node.terms:
            _roll_tree(term, rng, groups)
    elif isinstance(node, _Bin):
        _roll_tree(node.left, rng, groups)
        _roll_tree(node.right, rng, groups)
    elif isinstance(node, _Neg):
        _roll_tree(node.operand, rng, groups)
    elif isinstance(node, _Func):
        _roll_tree(node.arg, rng, groups)


def _fmt_num(v: float | int) -> str:
    if isinstance(v, float) and v.is_integer():
        return str(int(v))
    return str(v)


def _die_repr(d: DieRoll, coin: bool) -> str:
    if coin:
        inner = "T" if d.value == 0 else "H"
    elif d.reroll_history is not None and len(d.reroll_history) > 1:
        seq = (
            d.reroll_history
            if d.reroll_history[-1] == d.value
            else [*d.reroll_history, d.value]
        )
        inner = "→".join(map(str, seq))
    elif d.replaced_from is not None:
        inner = f"{d.replaced_from}→{d.value}"
    else:
        inner = str(d.value)
    if d.success is not None:
        inner += "✓" if d.success else "✗"
    if d.dropped:
        return f"~~{inner}~~"
    return inner


def _apply_arith(op: str, a: float | int, b: float | int) -> float | int:
    if op == "+":
        return a + b
    if op == "-":
        return a - b
    if op == "*":
        return a * b
    if b == 0:
        raise RollError("錯誤：除以零")
    return a / b


def _paren_left(s: str, prec: int, p: int) -> str:
    return f"({s})" if prec < p else s


def _paren_right(s: str, prec: int, p: int) -> str:
    return f"({s})" if prec <= p else s


def _eval(node: object) -> tuple[float | int | _MultiValue, str]:
    if isinstance(node, _Num):
        return node.value, _fmt_num(node.value)
    if isinstance(node, _Neg):
        v, s = _eval(node.operand)
        if isinstance(v, _MultiValue):
            values = [-x for x in v.values]
            renders = [
                f"-{_paren_left(si, pr, 4)}" for si, pr in zip(v.renders, v.precs)
            ]
            return (
                _MultiValue(values, renders, [3] * len(values)),
                "{" + ", ".join(renders) + "}",
            )
        if _prec_of(node.operand) < 4:
            s = f"({s})"
        return -v, f"-{s}"
    if isinstance(node, _Func):
        v, s = _eval(node.arg)
        if isinstance(v, _MultiValue):
            outs: list[float | int] = []
            for x in v.values:
                if node.name == "floor":
                    outs.append(math.floor(x))
                elif node.name == "ceil":
                    outs.append(math.ceil(x))
                elif node.name == "round":
                    outs.append(round(x))
                else:
                    outs.append(abs(x))
            renders = [f"{node.name}({si})" for si in v.renders]
            return (
                _MultiValue(outs, renders, [4] * len(outs)),
                "{" + ", ".join(renders) + "}",
            )
        if node.name == "floor":
            out: float | int = math.floor(v)
        elif node.name == "ceil":
            out = math.ceil(v)
        elif node.name == "round":
            out = round(v)
        else:
            out = abs(v)
        return out, f"{node.name}({s})"
    if isinstance(node, _Dice):
        g = node.group
        assert g is not None  # _roll_tree ran first
        parts = [_die_repr(d, node.coin) for d in g.rolls]
        # contract: subtotal first, face list backtick-wrapped — `14`(6+6+1+1)`
        return g.total, (
            node.dyn_prefix + f"{_fmt_num(g.total)}" + "`(" + "+".join(parts) + ")`"
        )
    if isinstance(node, _Pool):
        evaluated = [_eval(t) for t in node.terms]
        # bare multi-group (no `}`-filter): keep each group's total — no silent sum
        if not node.mods and len(node.terms) > 1:
            values_m: list[float | int] = []
            renders_m: list[str] = []
            precs_m: list[int] = []
            for t, (ev, s) in zip(node.terms, evaluated, strict=True):
                if isinstance(ev, _MultiValue):
                    values_m.extend(ev.values)
                    renders_m.extend(ev.renders)
                    precs_m.extend(ev.precs)
                else:
                    values_m.append(ev)
                    renders_m.append(s)
                    precs_m.append(_prec_of(t))
            multi = _MultiValue(values_m, renders_m, precs_m)
            return multi, "{" + ", ".join(renders_m) + "}"
        values = [
            sum(ev.values) if isinstance(ev, _MultiValue) else ev
            for ev, _ in evaluated
        ]
        renders = [s for _, s in evaluated]
        kept = set(range(len(values)))
        for m in node.mods:
            if isinstance(m, _KdMod):
                _pool_keepdrop(values, kept, m.name, m.n)
        pool_total: float | int = sum(values[i] for i in kept)
        for m in node.mods:
            if isinstance(m, _CondMod) and m.kind in _COUNT_MOD_IDS:
                count = 0
                for i in sorted(kept):
                    v = values[i]
                    if m.kind == "even":
                        hit = v % 2 == 0
                    elif m.kind == "odd":
                        hit = v % 2 == 1
                    elif m.kind == "cf":
                        ok = m.cp is None or _cp_match(v, m.cp[0], m.cp[1])
                        hit = not ok
                    else:  # cs
                        hit = m.cp is None or _cp_match(v, m.cp[0], m.cp[1])
                    if hit:
                        count += 1
                pool_total = count  # last count mod wins (dice-layer parity)
        # bare numeric literals stay unmarked — §5 golden `{1d20,10}kh` renders plain 10
        term_renders = [
            renders[i]
            if i in kept or isinstance(node.terms[i], _Num)
            else f"~~{renders[i]}~~"
            for i in range(len(renders))
        ]
        return pool_total, "{" + ", ".join(term_renders) + "}" + node.mods_suffix
    if isinstance(node, _Bin):
        lv, ls = _eval(node.left)
        rv, rs = _eval(node.right)
        if isinstance(lv, _MultiValue) or isinstance(rv, _MultiValue):
            return _eval_bin_multi(node, lv, ls, rv, rs)
        p = _PREC[node.op]
        out = _apply_arith(node.op, lv, rv)
        if _prec_of(node.left) < p:
            ls = f"({ls})"
        if _prec_of(node.right) <= p:
            rs = f"({rs})"
        return out, f"{ls}{node.op}{rs}"
    raise RollError("錯誤：無法解析的公式：")  # unreachable


def _eval_bin_multi(
    node: _Bin,
    lv: float | int | _MultiValue,
    ls: str,
    rv: float | int | _MultiValue,
    rs: str,
) -> tuple[_MultiValue, str]:
    p = _PREC[node.op]
    op = node.op

    if isinstance(lv, _MultiValue) and isinstance(rv, _MultiValue):
        if len(lv.values) != len(rv.values):
            raise RollError("錯誤：骰池長度不同")
        values: list[float | int] = []
        renders: list[str] = []
        for i in range(len(lv.values)):
            a, b = lv.values[i], rv.values[i]
            left = _paren_left(lv.renders[i], lv.precs[i], p)
            right = _paren_right(rv.renders[i], rv.precs[i], p)
            values.append(_apply_arith(op, a, b))
            renders.append(f"{left}{op}{right}")
        return _MultiValue(values, renders, [p] * len(values)), "{" + ", ".join(
            renders
        ) + "}"

    if isinstance(lv, _MultiValue):
        right = _paren_right(rs, _prec_of(node.right), p)
        values = [_apply_arith(op, v, rv) for v in lv.values]  # type: ignore[arg-type]
        renders = [f"{_paren_left(s, pr, p)}{op}{right}" for s, pr in zip(lv.renders, lv.precs)]
        return _MultiValue(values, renders, [p] * len(values)), "{" + ", ".join(
            renders
        ) + "}"

    assert isinstance(rv, _MultiValue)
    left = _paren_left(ls, _prec_of(node.left), p)
    values = [_apply_arith(op, lv, v) for v in rv.values]  # type: ignore[arg-type]
    renders = [f"{left}{op}{_paren_right(s, pr, p)}" for s, pr in zip(rv.renders, rv.precs)]
    return _MultiValue(values, renders, [p] * len(values)), "{" + ", ".join(
        renders
    ) + "}"


def _prec_of(node: object) -> int:
    if isinstance(node, _Bin):
        return _PREC[node.op]
    if isinstance(node, _Neg):
        return 3
    return 4  # _Num / _Func / _Dice / _Pool (already parenthesised)


def split_formula_comment(src: str) -> tuple[str, str]:
    """Split ``src`` into (formula, comment).

    ``formula`` = the longest prefix of ``src`` that :func:`roll` can parse
    (whitespace-stripped); ``comment`` = the remainder (may be ``""``).
    This lets commands like `,r {4d6, 3d8}kh 註解` and `,r 2d6 註解` work:
    the formula may contain spaces inside pool braces ``{4d6, 3d8}``; the
    split is done by "longest parseable prefix" so a trailing 註解/comment
    (separated by spaces) is never swallowed into the formula. ``#`` is a
    legacy comment marker and is handled by ``roll()`` yourself, but this
    function is the space/whitespace variant.

    Returns (formula, comment). Raises ``RollError`` if no prefix of ``src``
    parses.
    """
    src_stripped = src.strip()
    if not src_stripped:
        raise RollError(f"錯誤：無法解析的公式：{src}")

    try:
        roll(src_stripped)
        return (src_stripped, "")
    except RollError:
        pass

    i = len(src_stripped)
    while i > 0:
        space_pos = src_stripped.rfind(" ", 0, i)
        if space_pos == -1:
            break
        prefix = src_stripped[:space_pos].rstrip()
        if not prefix:
            break
        try:
            roll(prefix)
            comment = src_stripped[space_pos + 1 :].lstrip()
            return (prefix, comment)
        except RollError:
            i = space_pos

    raise RollError(f"錯誤：無法解析的公式：{src}")


def roll(formula: str, rng: random.Random | None = None) -> RollResult:
    """Parse + validate + roll ``formula`` (Foundry grammar subset).

    ``rng=None`` uses the module-level default RNG; tests inject
    ``random.Random(seed)``.
    """
    if rng is None:
        rng = _DEFAULT_RNG
    if len(formula) > 512:
        raise RollError("錯誤：公式過長")
    src = formula.split("#", 1)[0].strip()
    if not src:
        raise RollError(f"錯誤：無法解析的公式：{formula}")

    toks = _tokenize(src)
    expr, check = _Parser(toks, src).parse_roll()

    groups: list[DiceGroup] = []
    _roll_tree(expr, rng, groups)
    total_ev, display = _eval(expr)
    parts: list[str] | None = None
    if isinstance(total_ev, _MultiValue):
        total: float | int = sum(total_ev.values)
        parts = [
            render
            if _self_contained(render, value)
            else f"{render} = {_fmt_num(value)}"
            for render, value in zip(total_ev.renders, total_ev.values, strict=True)
        ]
    else:
        total = total_ev

    check_res: CheckResult | None = None
    if check is not None:
        op, target = check
        if op == ">":
            passed = total > target
        elif op == ">=":
            passed = total >= target
        elif op == "<":
            passed = total < target
        else:
            passed = total <= target
        check_res = CheckResult(op, target, passed)

    if groups or check_res is not None:
        base = display
    else:
        base = src
    if check_res is not None:
        verdict = "成功" if check_res.passed else "失敗"
        terms = f"{base} {check_res.op} {check_res.target} → {verdict}"
    else:
        terms = base

    return RollResult(
        formula=src,
        total=total,
        terms=terms,
        check=check_res,
        groups=groups,
        parts=parts,
    )


def _self_contained(terms: str, total: float | int) -> bool:
    """True when ``terms`` already carries its own ``{total}``(`faces`)`` shape."""
    t = _fmt_num(total)
    return (
        terms.startswith(f"{t}`")
        and terms.endswith("`")
        and terms.count("`") == 2
    )


def format_result(rr: RollResult) -> str:
    """Render a RollResult for Discord (inline-code spans never nest).

    Shapes:
      no check, single group: ``14`(6+6+1+1)` ``
      no check, multi:        ``14`(6+6+1+1)`+3+2`(2)` = 19``
      no check, bare pool:    one line per group (broadcast → ``… +5 = 17``)
      with check:             ``11`(11)`+2 = 13 >= 15 → 失敗``
    `` = {total}`` is appended only when ``terms`` is not a single
    self-contained group; whole-term outer backticks are never added.
    """
    if rr.check is None:
        terms = rr.terms.rstrip()
        if rr.parts is not None:
            return "\n".join(rr.parts)
        if _self_contained(terms, rr.total):
            return terms
        return f"{terms} = {_fmt_num(rr.total)}"
    suffix = (
        f" {rr.check.op} {rr.check.target} → "
        f"{'成功' if rr.check.passed else '失敗'}"
    )
    base = rr.terms[: -len(suffix)] if rr.terms.endswith(suffix) else rr.terms
    base = base.rstrip()
    if _self_contained(base, rr.total):
        return f"{base}{suffix}"
    return f"{base} = {_fmt_num(rr.total)}{suffix}"
