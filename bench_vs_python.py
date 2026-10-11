#!/usr/bin/env python3
"""Python 舊引擎 vs Rust 新引擎的實測對比。

用法：
    .venv/bin/python abgrund-dice/bench_vs_python.py

兩邊跑**同一組公式、同一個 seed**，所以是同等條件的對比。
Rust 端走 CSPRNG（正式路徑）——注入 RNG 會多一次 GIL 往返，這裡不模擬。

量測方法：warm-up 後取多批次的中位數（避免單次抖動）。
"""

from __future__ import annotations

import importlib.util
import random
import statistics
import sys
import timeit
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from modules.RollingModule import RollError as RsRollError  # noqa: E402
from modules.RollingModule import format_result as rs_format  # noqa: E402
from modules.RollingModule import roll as rs_roll  # noqa: E402


def _load_oracle():
    path = Path(__file__).parent / "reference" / "RollingModule_py.py"
    spec = importlib.util.spec_from_file_location("rolling_py_oracle", path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


oracle = _load_oracle()


def py_roll(formula: str) -> str:
    return oracle.format_result(oracle.roll(formula, random.Random(0)))


def rs_roll_fmt(formula: str) -> str:
    return rs_format(rs_roll(formula))


def time_median(fn, repeats: int, batches: int = 5) -> float:
    """回傳「單次」的秒數中位數。"""
    fn()  # warm-up
    samples = []
    for _ in range(batches):
        t0 = timeit.default_timer()
        for _ in range(repeats):
            fn()
        samples.append((timeit.default_timer() - t0) / repeats)
    return statistics.median(samples)


def dice_count(formula: str) -> int:
    try:
        return sum(len(g.rolls) for g in rs_roll(formula).groups)
    except RsRollError:
        return -1


# ---------------------------------------------------------------------------
# 案例
# ---------------------------------------------------------------------------


def pool(n: int, term: str, sep: str = ",") -> str:
    return "{" + sep.join([term] * n) + "}"


def chained(n: int, term: str) -> str:
    return "+".join([term] * n)


SINGLE = [
    ("一般擲骰", "4d6k3+2d8kh+1d20>=15"),
    ("骰池 8 組", pool(8, "100d6")),
    ("骰池 85 組（上限附近）", pool(85, "100d6")),
    ("爆炸飽和", "100d6x<=6"),
    ("爆炸池 46 組", chained(46, "100d6x<=6")),
    ("遞迴重骰", "100d6rr<6"),
    ("重骰池 8 組", pool(8, "100d6rr<6")),
    ("最大骰面", "100d100000"),
    ("重骰鏈 5 層", "100d6r<6r<5r<4r<3r<2"),
    ("極限 73000 顆骰", chained(73, "100d1x")),
]

MULTI = [
    (",r 50  4d6k3", "4d6k3"),
    (",r 50  骰池 46 組", chained(46, "100d6x<=6")),
    (",r 50  73000 顆骰", chained(73, "100d1x")),
    (",r 50  骰池 85 組", pool(85, "100d6")),
]


def main() -> None:
    print("=" * 96)
    print("單次擲骰（含渲染）")
    print("=" * 96)
    print(f"{'案例':26} {'骰數':>7} {'Python':>11} {'Rust':>10} {'倍數':>8} {'Rust µs/骰':>11}")
    print("-" * 96)

    for label, formula in SINGLE:
        if len(formula) > 512:
            print(f"{label:26} 跳過（{len(formula)} 字元）")
            continue
        n = dice_count(formula)
        reps = 2 if n > 40_000 else (5 if n > 5_000 else 200)
        tp = time_median(lambda f=formula: py_roll(f), reps)
        tr = time_median(lambda f=formula: rs_roll_fmt(f), reps)
        per = (tr / n * 1e6) if n > 0 else float("nan")
        print(
            f"{label:26} {n:7} {tp*1e3:9.2f}ms {tr*1e3:8.3f}ms "
            f"{tp/tr:7.1f}x {per:10.3f}"
        )

    print()
    print("=" * 96)
    print("`,r 50 <公式>` —— cogs/diceRoll.py 的多次擲骰路徑（這就是「1.5 秒」的來源）")
    print("=" * 96)
    print(f"{'案例':26} {'骰數/次':>8} {'Python':>12} {'Rust':>10} {'倍數':>8}")
    print("-" * 96)

    for label, formula in MULTI:
        if len(formula) > 512:
            print(f"{label:26} 跳過（{len(formula)} 字元）")
            continue
        n = dice_count(formula)
        reps = 1 if n > 40_000 else (3 if n > 5_000 else 20)

        def py50(f=formula):
            for _ in range(50):
                py_roll(f)

        def rs50(f=formula):
            for _ in range(50):
                rs_roll_fmt(f)

        tp = time_median(py50, reps, batches=3)
        tr = time_median(rs50, reps, batches=3)
        print(
            f"{label:26} {n:8} {tp*1e3:10.0f}ms {tr*1e3:8.0f}ms {tp/tr:7.1f}x"
        )

    print()
    print("=" * 96)
    print("解析／驗證（不擲骰）—— split_formula_comment 會對每個空白前綴真的擲一次")
    print("=" * 96)
    heavy = pool(40, "100d1x") + " 攻擊擲骰"
    for label, src in [("短公式 + 註解", "2d6 註解"), ("重骰池 + 註解", heavy)]:
        tp = time_median(lambda s=src: oracle.split_formula_comment(s), 20, batches=3)
        tr = time_median(lambda s=src: __import__(
            "modules.RollingModule", fromlist=["split_formula_comment"]
        ).split_formula_comment(s), 200, batches=3)
        print(f"{label:26} {len(src):6} 字元  Python {tp*1e3:8.2f}ms  Rust {tr*1e3:7.3f}ms  {tp/tr:6.1f}x")

    print()
    print("註：Rust 走 CSPRNG（正式路徑）。注入 random.Random 會多一次 GIL 往返／骰。")


if __name__ == "__main__":
    main()
