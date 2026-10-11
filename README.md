# abgrund-dice

A Rust dice‑rolling engine with PyO3 bindings, implementing a Foundry VTT‑like grammar.

## Features

- Parse and evaluate dice formulas (e.g. `4d6k3+2d8kh+1d20>=15`).
- Single‑pass evaluator: rolls dice and builds the output string in one traversal.
- Rich modifiers: keep/drop, reroll, explode, min/max, success/failure counts, margin success.
- Dice pools, dynamic dice, arithmetic, functions (`floor`, `ceil`, `round`, `abs`).
- Deterministic RNG injection for testing (`random.Random(seed)`).
- Error messages include source location (inline bold for parse errors, arrow block for semantic errors).
- Optional i18n: error messages can be switched between Traditional Chinese and English via `set_language`.
- No runtime panic on any input (guaranteed by fuzz testing).

## Usage (Python)

```python
import abgrund_dice

# Default: Traditional Chinese error messages
result = abgrund_dice.roll("1d20+5")
print(result)          # RollResult(formula='1d20+5', total=..., terms='...')
print(abgrund_dice.format_result(result))

# Switch to English (optional)
abgrund_dice.set_language("en")
print(abgrund_dice.roll("1d4+100dx<=6 + 1d20 > 3000"))
# → Error: Unable to parse formula: `1d4 + ***100dx<=6*** + 1d20 > 3000`
```

### API

- `roll(formula: str, rng: Optional[object]) -> RollResult`  
  Parse, validate, and roll. `rng=None` uses the internal ChaCha12‑based CSPRNG.  
  Any object with a `randint(lo, hi)` method (e.g. `random.Random(seed)`) can be injected for deterministic results.

- `format_result(rr: RollResult) -> str`  
  Render the roll result as a Discord‑ready string (inline code span, optional `= total`).

- `split_formula_comment(src: str) -> Tuple[str, str]`  
  Separate a command string into the longest parsable formula prefix and the trailing comment.

- `set_language(lang: str)` – `"zh_TW"` (default) or `"en"`.

- `RollError` exception – contains:
  - `message`: localized error text (matches historic Python version).
  - `span`: `(start, end)` byte offsets inside the stripped formula (if known).
  - `pointer`: two‑line visual cue (formula + aligned `^^^`).

Result dataclasses (`RollResult`, `DiceGroup`, `DieRoll`, `CheckResult`) are exposed as plain Python objects with read‑only attributes.

## Building

The crate is a standard Rust library with PyO3 bindings.

```bash
# Development rebuild (fast)
cd abgrund-dice
maturin develop --uv --release

# Build a distributable wheel
maturin build --uv --release
```

The wheel can be installed with `pip install abgrund_dice‑<version>-*.whl`.

## Testing

```bash
# Rust unit tests
cargo test
# Lint
cargo clippy --all-targets
# Differential test against the frozen Python oracle
python -m pytest tests/test_rolling_differential.py
```

## Implementation Overview (≈3 min read)

1. **Lexer** – `logos` produces `SpannedToken` instances that carry byte offsets, enabling precise error location.
2. **Parser** – `winnow` combinators implement the expression grammar (`expr := term (('+'|'-') term)*`, etc.). The parser returns an AST (`Expr`) and an optional check condition.
3. **AST** – A small set of enums (`Expr`, `DiceNode`, `PoolNode`, `DiceMod`, …). The `DiceKind` enum (`Normal`, `Fate`, `Coin{call}`) replaces the old ambiguous `faces == 0` sentinel.
4. **Evaluator** – A single‑pass walk that simultaneously rolls dice, applies modifiers (`reroll → explode → min/max → keep/drop → sum → count → deduct → margin`) and builds the render string. No separate roll‑then‑eval pass, preserving the original order of RNG consumption.
5. **Error handling** – All errors are `RollError`. Parse errors store the offending span and render it with inline bold (`***…***`). Semantic errors add an arrow block. The `Display` implementation yields the historic message; i18n is provided via a simple lookup table switched at runtime.
6. **PyO3 Layer** – Thin binding (`src/lib.rs`) that converts between Rust types and Python objects, exposing the four public functions and the exception class. No business logic lives here.

## Design Principles

- **Good Taste** – Eliminated special cases (e.g., the old `faces == 0` ambiguity) by using an explicit enum.
- **Never Break Userspace** – Public API (`roll`, `format_result`, `split_formula_comment`, `RollError`) and error message texts remain identical to the original Python module (modulo optional language switch).
- **Pragmatism** – Every line solves a real problem; no unused abstractions or speculative generality.
- **Obsession with Simplicity** – Functions are short, data structures are minimal, and the evaluator avoids unnecessary allocations (e.g., `Part` bundles value, render, and precedence).

---

*This README is intended as a quick start guide. For full details, see the source code and the test suite.*
