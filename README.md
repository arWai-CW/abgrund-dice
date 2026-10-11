# abgrund-dice — 骰子引擎（Rust + PyO3）

舊 `modules/RollingModule.py` 的 Rust 重寫。公開 API 完全不變，
`modules/RollingModule.py` 現在只是轉出這一層。

```
logos (lexer)  →  winnow (parser)  →  AST  →  單趟評估器  →  PyO3  →  Python
```

---

## 為什麼是這個架構

### 舊 Python 本來就是 Lexer + AST + 評估器

`RollingModule.py` 已經有三層：`_tokenize`（regex lexer）、`_Parser`
（遞迴下降）、`_eval` / `_execute_dice`（評估器）。所以「改成 Lexer + AST
評估器」不是架構變更——變更的是**實作語言**與 **lexer/parser 的技術選擇**。

真正換掉的東西：

| | 舊 Python | Rust |
|---|---|---|
| lexer | 手寫 `re` 迴圈、`_Tok` 帶 `start/end` | `logos` derive，編譯期驗證 pattern |
| parser | 手寫遞迴下降、`self.i` / `self.last_end` 書籤 | `winnow` combinators，位置由 `Location` 提供 |
| 原始碼切片 | 手動維護 `last_end`（最脆弱處） | token 自帶 byte offset，`notation` 直接切片 |
| AST | `__slots__` 類別 + `isinstance` 鏈 | `enum`，`match` 窮舉 |
| 漏掉的 AST 分支 | 靜默走到 `raise ... # unreachable` | 編譯不過 |
| RNG | `random.Random`（MT19937） | `ThreadRng`（ChaCha12 + OS 熵源） |
| 骰型 | `faces == 0` 當 sentinel（命運骰與硬幣**無法區分**） | `DiceGroup.kind` 自我描述 |
| 錯誤 | 只有訊息 | 訊息 + **出錯位置**（Parse 行內粗斜體／其他插入符指示） |
| 遞迴上限 | 必要——CPython 會 `RecursionError` | 純粹防 stack overflow，行為對齊 |

### 模組地圖

| 檔案 | 職責 |
|---|---|
| `src/lexer.rs` | `logos` 掃描 → `SpannedToken`（自帶 byte offset，兼作 winnow token） |
| `src/ast.rs` | `Expr` / `DiceNode` / `DiceMod` / `PoolNode` |
| `src/parser.rs` | `winnow` parser（token stream → AST）+ 解析期驗證 |
| `src/evaluator.rs` | 單趟評估：擲骰 + 渲染 |
| `src/rng.rs` | `DiceRng` 抽象：`CryptoRng` / `SeededRng` |
| `src/engine.rs` | `roll()` / `format_result()` / `split_formula_comment()`（純 Rust） |
| `src/lib.rs` | PyO3 綁定，只做型別轉換 |
| `reference/RollingModule_py.py` | **凍結的 oracle**，只給差分測試用 |

### winnow 用在對的地方

表達式文法用 combinator（`repeat().fold()` 做左結合鏈、
`separated()` 做骰池項列），修飾詞後綴用 `match` 分派——因為每個修飾詞的
**arity 與形狀都不同**（`x5=10` 是「次數上限 + 條件」、`kh3` 是「數目」、
`ms10` 是「必要數目」），用 `match` 就是最清楚的 dispatch。硬套
combinator 只會更難讀。

### AST 評估器為什麼是「單趟」

舊版是兩趟：`_roll_tree` 先擲完所有骰子（消耗 RNG），`_eval` 再算值與字串。
兩趟的走訪順序**完全相同**（left-to-right、inner-before-outer），而 `_eval`
不消耗 RNG——所以合併成單趟後，RNG 消耗順序與渲染輸出都不變。
（見 `tests/test_rolling_differential.py` 對 `(1d20*2)d(1d10)` 這類
動態骰的逐顆比對。）

---

## 建置

```bash
# 正常路徑：uv sync 就會編（release profile）
uv sync --reinstall-package abgrund-dice

# Rust 開發迴圈（較快，省掉 uv 的 wheel 打包）
cd abgrund-dice && VIRTUAL_ENV=../.venv maturin develop --uv --release
```

> ⚠️ **`uv sync` 不會偵測 `.rs` 變更。**
> uv 對 path dependency 的 freshness 只看 metadata，改了 Rust 原始碼後
> 直接 `uv sync` 會**靜默沿用舊的 `.so`**。請用
> `uv sync --reinstall-package abgrund-dice`（`deploy.sh` 已這樣做）。
> cargo 是增量編譯，原始碼沒動時幾乎不花時間。

需要 Rust toolchain（`cargo`）：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

---

## 測試

```bash
cargo test                              # Rust 層（39 個）
cargo clippy --all-targets              # 必須零警告
.venv/bin/python -m pytest tests/test_rolling_differential.py   # 主要證據
```

### 四層防護

1. **`tests/test_rolling_engine.py`（125 個）** — 既有規格測試，一行未改。
2. **`tests/test_rolling_differential.py`（1300+ 個）** — 把整個語法空間
   （約 460 條公式 × 8 個 seed）與 `reference/RollingModule_py.py` 逐欄位
   對拍：每顆骰的 flag / 重骰鏈 / 爆炸深度、渲染字串、`parts`、例外訊息。
   **這是移植正確性的主要證據。**
3. **`abgrund-dice/tests/engine.rs`（41 個）** — 補 Python 測不到的面相：
   CSPRNG 路徑、`SeededRng` 可重現性、內部不變量、stack overflow /
   長度上限、int/float 身分、值域不變量、錯誤位置。
4. **`abgrund-dice/tests/fuzz.rs`（5 個，約 22 萬案例）** — 文法感知 + 突變 +
   純亂碼，斷言 `roll()` **永不 panic**，並驗證錯誤指示塊的結構不變量。

> **為什麼「不 panic」是契約**：Rust panic 會變成 PyO3 的 `PanicException`，
> 而它是 `BaseException` 的直系子類——`cogs/diceRoll.py` 的
> `except RollError` **攔不到**，會變成未處理例外。舊 Python 沒有這個曝露面。
> 所以「任意輸入都不 panic」是 Rust 改寫**新引進**的對外契約，必須有測試守著。

oracle 凍結於 git `c1ffc07`。**不要修改它**——改了就不再是 oracle。

---

## 隨機源

```python
roll("4d6")                    # Rust 內建 CSPRNG（ThreadRng，ChaCha12）
roll("4d6", random.Random(0))  # 注入：走它的 randint，決定性（測試用）
roll("4d6", random.SystemRandom())
```

舊版用 `random.Random`（MT19937）。MT19937 的完整內部狀態可以從 624 個
連續 32-bit 輸出還原——對公開 bot 來說是可預測的骰子。`ThreadRng` 是
ChaCha12 + OS 熵源播種，屬密碼學安全隨機，且每顆骰子不需跨 GIL。

任何有 `randint(lo, hi)` 的物件都能注入（`Crypto.Random`、`SystemRandom`…）；
沒有 `randint` 會拿到明確的 `TypeError`，注入物件的例外原樣傳遞。

---

## 錯誤位置指示

公式很長、組合又多，只說「無法解析」幫不上忙。所有**解析期**錯誤都會指出
出錯的字元。兩種形狀：

**無法解析的公式**（公式就嵌在訊息本體裡）——出錯的部分包粗斜體，其餘包
行內 code span：

```
錯誤：無法解析的公式：`1d4 + `***100dx<=6***` + 1d20 > 3000`
```

**其他錯誤**（語意錯誤）——訊息 + 圍籬內的指示塊：

```
錯誤：骰面必須介於 1~100000

```
{4d6, 3d8}kh+1d0
               ^
```
```

Parse 的行內版**不附**箭頭區塊：出錯部分已經標出來了，同一件事不說兩次。
`.pointer` 仍然算得出來（結構化欄位，供程式取用）。

Python 端的 `RollError`：

| 欄位 | 內容 |
|---|---|
| `str(e)` | 訊息本體（Parse 帶行內標記／其他帶圍籬指示塊）；`cogs/diceRoll.py` 直接送這個 |
| `e.message` | 訊息本體，**與舊版逐字相同**（差分測試比對的就是它） |
| `e.span` | `(start, end)` byte 位移（相對剝離註解後的公式） |
| `e.pointer` | 兩行純文字（公式 + 對齊插入符）；沒有位置時為 `""` |

實作要點：

- **一定要把公式包住**：公式可能含 `*` 或 `_`，裸露在 Discord 會被當成
  斜體／粗體標記而吃字。Parse 用行內 code span，其他用圍籬。
- **出錯部分的粗斜體會被讓掉**：該段含 `*` / `_` / 反引號時只包 code span
  ——那些字元會讓 `***` 破版，寧可沒有粗斜體也不要顯示錯亂。
- **插入符用字元位移**（不是 byte 位移）。這是安全的：lexer 只接受 ASCII，
  所以失敗點之前必然是 ASCII；公式裡的 CJK 只會是「第一個非法字元」，
  而它就是失敗點本身。
- **長公式會裁切**：箭頭區塊超過 100 字元只顯示錯誤附近的視窗（前後加
  `…`），否則 512 字元的公式會撐爆 Discord 的 2000 字元上限。行內版不裁
  （512 字元塞得進 2000）。
- **執行期錯誤沒有位置**：除以零、動態骰面超界是在擲骰時才發現的，
  指不出單一位置，此時 `span = None`、`pointer = ""`、`str(e)` 只有訊息。
- **拚錯的骰子項整體都是問題**：`100dx`、`4fdx`、`4kh3` 這種「數字後面
  接了不是骰子產生式的 ident」本來就一定是語法錯誤，但舊做法只指出落單的
  ident（`100dx` 只報 `dx`，沒有空白時整條尾巴都算進去）。現在從數字開始，
  連後面的比較條件一起當出錯範圍（`100dx<=6`）。
- **`,r <打錯的 token>` 也有位置**：`split_formula_comment` 整體失敗時，
  前綴是由後往前砍的、第一個詞是最後一個候選——它也不行，問題就落在第一個
  詞裡，位置從它身上拿。語法合法但擲不出來（`1d0`、`1/0`）仍指不出位置，
  訊息與舊版相同。

---

## 明細隱藏（訊息長度）

一組骰子的**實際**骰數超過 `MAX_DICE_DETAIL`（20）時，不列每顆骰的明細，
只給「總計 + 骰組規格」：

| 公式 | 舊渲染 | 新渲染 |
|---|---|---|
| `21d100` | ``74`(4+4+1+…+5)` ``（66 字元） | ``74`(21d100)` ``（14 字元） |
| `100d6x<=6` | ``3445`(1+…+6)` ``（3003 字元） | ``3445`(4d6x<=6)` ``（17 字元） |

**門檻比的是「實際骰出的顆數」，不是宣告的顆數。** `4d6x<=6` 宣告 4 顆卻會
爆炸出 1000 顆，用宣告值判斷就完全擋不住。反過來 `4d6k1` 擲 4 顆留 1 顆，
仍照列明細。

維持 `總計\`(…)\`` 的形狀是刻意的：`self_contained` 才認得出來，
`format_result` 就不會多補一個 `= 總計`。

效果（`,r 50`）：

| 公式 | 舊版 | | 新版 | |
|---|---:|---|---:|---|
| `{100d6}` | 10,340 | ❌ 400 | 857 | ✅ 1 則 |
| `{100d6 ×8}` | 82,790 | ❌ 400 | 1,989 | ✅ 4 則 |
| `{100d6 ×85}` | 879,740 | ❌ 400 | — | ❌ 需要 58 則（上限 10） |

骰值、旗標、重骰鏈等**語意層完全沒變**；只是不顯示。差分測試因此拆成兩層：
`semantics` 永遠逐欄位嚴格比對，`render` 只在這個規則適用時允許分歧。

---

## 已知與舊版的分歧

### 1. `min` / `max` 的值域檢查（新規則）

舊版會**靜默擲出界外值**：`1d1min2` 在 d1 上給出 2，`4d6max0` 在 d6 上給出 0。
`min` / `max` 的參數原本只驗證 `0~100000`（不是骰面），所以擋不住。

新規則：**只擋「會把骰值推出值域」的參數**。

| 公式 | 結果 | 理由 |
|---|---|---|
| `1d1min2` | `錯誤：min 不可大於 1` | min 抬到 2，超過 d1 的上界 |
| `2d2rr>=6min3` | `錯誤：min 不可大於 2` | 同上 |
| `4d6max0` | `錯誤：max 不可小於 1` | max 壓到 0，低於下界 |
| `4dc min2` | `錯誤：min 不可大於 1` | 硬幣值域是 `[0,1]`，不是 `[1,faces]` |
| `4d6max8` | OK（無害 no-op） | max 抬不過任何值，**不擋** |
| `4d6min0` | OK（無害 no-op） | min 降不過任何值，**不擋** |
| `4d6rr<3min2` | OK（min 自然失效） | reroll 退出條件已保證 `>= 3` |

刻意**不對稱**：只有真的會把值推出去的方向才擋。無害 no-op 必須放行，
否則長公式會動不動被誤擋——那正是打錯字最多的情境。

這個規則讓「骰值永不超版面值域」成為可強制的不變量，模糊測試因此能用
精確值域斷言（而不是 `[-1, 100000]` 這種弱界線）。

### 2. 超出 `f64` 精確度的整數字面值會被拒收

```
999999999999999    → 兩邊一致
9007199254740992   → 兩邊一致（2**53，仍是 f64 精確邊界內）
9007199254740993   → 舊版回 9007199254740993
                     新版回「錯誤：無法解析的公式：…」
```

舊版用 Python 大整數；新版以 `f64` 承載數值。若照收，`9999999999999999`
會**靜默**變成 `10000000000000000`。因此選擇大聲拒絕——靜默算錯比拒絕更糟。

這個文法的重要數值欄位（骰數 1~100、骰面 1~100000、修飾詞參數 0~100000）
全部遠小於此界線，16 位以上整數不可能是合法輸入，實際不會誤傷任何公式。
帶小數點的字面值不受影響（Python 也走 `float()`）。此行為有專門測試鎖住
（`test_int_literals_beyond_f64_precision_are_rejected_loudly`）。

---

## 效能

實測：`.venv/bin/python abgrund-dice/bench_vs_python.py`（同一台機器、同一公式、同一 seed，
Rust 走正式 CSPRNG 路徑，取多批次中位數）。

### 單次擲骰（含渲染）

| 案例 | 骰數 | Python | Rust | 倍數 | Rust µs/骰 |
|---|---:|---:|---:|---:|---:|
| 一般擲骰 `4d6k3+2d8kh+1d20>=15` | 7 | 0.03 ms | 0.002 ms | 10.7x | 0.34 |
| 骰池 8 組 | 800 | 0.33 ms | 0.012 ms | 26.3x | 0.015 |
| 骰池 85 組（512 字元上限附近） | 8500 | 3.66 ms | 0.125 ms | 29.3x | 0.015 |
| 爆炸飽和 `100d6x<=6` | 1000 | 0.51 ms | 0.012 ms | 42.9x | 0.012 |
| 爆炸池 46 組 | 46000 | 26.71 ms | 0.550 ms | 48.6x | 0.012 |
| 遞迴重骰 `100d6rr<6` | 100 | 0.31 ms | 0.011 ms | 27.1x | 0.114 |
| 重骰池 8 組 | 800 | 2.40 ms | 0.092 ms | 26.0x | 0.115 |
| 極限 73000 顆骰 `100d1x ×73` | 73000 | 43.43 ms | 0.817 ms | 53.2x | 0.011 |

### `,r 50 <公式>` —— 這就是「感覺 1.5 秒」的來源

| 案例 | 骰數/次 | Python | Rust | 倍數 |
|---|---:|---:|---:|---:|
| `,r 50 4d6k3` | 4 | 1 ms | 0.4 ms | 14.1x |
| `,r 50 {100d6x<=6 ×46}` | 46000 | **1346 ms** | **26 ms** | 51.4x |
| `,r 50 100d1x ×73` | 73000 | 2228 ms | 42 ms | 52.9x |
| `,r 50 {100d6 ×85}` | 8500 | 192 ms | 8 ms | 25.0x |

### 讀法

- **固定開銷很小，所以倍數隨骰數上升**：7 顆骰 10.7x，8500 顆 29.3x，
  46000 顆 48.6x。大公式才是重寫真正賺到的地方。
- **每顆骰的成本**（比倍數更有意義）：Rust 約 **12 ns/骰**，舊 Python 約 **450 ns/骰**。
- **重骰仍是相對較貴的路徑**（114 ns/骰 vs 一般 12 ns/骰）：每顆被重骰的骰子
  都要配一個 `Vec<i64>` 存重骰鏈。最壞情況 0.092 ms，屬理論優化，不做。
- **`split_formula_comment` 會「真的擲骰」來測能否解析**——這是繼承自舊版 oracle
  的設計（要連執行期錯誤如 `1/0` 一起判定），兩邊都浪費。
  286 字元的重骰池 + 註解：Python 24.35 ms → Rust 0.995 ms（24.5x），
  Rust 端取自帶 seed 的探測 RNG，不消耗正式熵源。

> **這些數字仍然不是重寫的理由。** 正式環境的延遲是 Discord API 往返
> （數十至數百毫秒）。`4d6` 從 30 µs 降到 2 µs 等於 0.001%。
> 唯一真的有感的是**退化輸入**——例如 `,r 50 {100d6x<=6 ×46}` 從 1.4 秒
> 降到 78 ms。重寫真正的價值仍是那 1300 條差分測試。

`cargo run --release --example bench` 可重跑。

**不過這不是重寫的理由。** 正式環境的延遲是 Discord API 往返
（數十至數百毫秒）；每次擲骰省下 15 µs 等於 0.02%。重寫真正的價值在
正確性保證：窮舉的 AST、結構化的原始碼切片、以及那 1164 個差分測試
——它們讓之後擴充文法時不會踩到舊行為。

### 並行化？量過了，是錯的工具

`cargo run --release --example profile` 拆解最壞情況（46 組爆炸池，46,000 顆骰）：

| 階段 | 時間 | 佔比 |
|---|---:|---:|
| RNG 46,000 次抽取 | 0.47 ms | 25% |
| **為每顆骰配一個 `String`** | **0.95 ms** | **51%** |
| 每組 join 1000 + 外層 join 46 | 0.14 ms | 7% |
| 完整 `roll()` | 1.88 ms | 100% |

瓶頸是**配置**，不是 CPU。`cargo run --release --example parallel_experiment` 四條路實測：

| 方案 | 時間 | 相對現況 |
|---|---:|---:|
| A. 循序（現況） | 1.840 ms | — |
| B. rayon 並行（每組一個工作單元，10 執行緒） | 0.503 ms | 3.66x |
| C. 循序 + 共用緩衝（不為每顆骰配 `String`） | **0.210 ms** | **8.77x** |
| D. 並行 + 共用緩衝 | 0.238 ms | 7.74x |

**C 比 B 好 2.4 倍，而且零依賴、零非決定性。D 比 C 差**——配置器跨執行緒競爭
加上執行緒喚醒成本，把並行的收益吃掉了。

而且最後**根本不需要 C**：只要在骰數 > `MAX_DICE_DETAIL` 時不列明細，
那些 `String` 就從來沒被配置。同一個案例 1.840 ms → **0.550 ms**，
每顆骰從 31 ns 降到 12 ns，比 B（並行）與 C（共用緩衝）都好，而且程式碼更短。

**結論：先問「這些工作需不需要做」，再問「怎麼做更快」。**

**而且這些優化全都用不到**，因為會超過 1 ms 的輸入一律送不出去：

| 重骰池 | 骰數 | 輸出字元 | Discord 2000 上限 | Rust |
|---|---:|---:|---|---:|
| 1 組 | 1000 | 2007 | ❌ 被拒 | 0.03 ms |
| 8 組 | 8000 | 16071 | ❌ 被拒 | 0.24 ms |
| 46 組 | 46000 | 92376 | ❌ 被拒 | 1.53 ms |

**送得出去的最大重骰池花 0.24 ms。** 1.53 ms 那筆根本到不了使用者眼前。

架構上並行還有三個障礙：

1. **決定性注入無法並行**——`random.Random(seed)` 是單一循序串流且需要 GIL。
   並行只能給 CSPRNG 路徑 → 會多出第二條評估路徑（就是本專案一直在避免的
   「兩份真相」）。
2. **動態骰有真實依賴**——`1d(1d20)` 的內層必須先擲完，外層才知道 `faces`。
   這是樹狀依賴，只有獨立的兄弟節點能並行，不是 `par_iter` 一行能解決。
3. **GIL 仍被持有**——worker 不碰 Python 才安全，所以這是「單次延遲」優化，
   不是吞吐量優化。

> 此實驗用的 `rayon` 只在 `[dev-dependencies]`，**不進 shipped wheel**
> （`cargo clippy`/`cargo test` 會編它，wheel 不會連它）。

### 一個既有的周邊風險（不是本次改寫造成的）

重骰池會產生極長的訊息，而 `cogs/diceRoll.py` 沒有切訊息：

| 公式 | 輸出長度 | Discord 上限 2000 |
|---|---:|---|
| `4d6k3` | 17 字元 | ✅ |
| `{100d6 ×8}` | 1655 字元 | ✅（剛好過） |
| `{100d6x<=6 ×46}` | 92,367 字元 | ❌ 會被 API 拒絕 |

舊引擎輸出一樣長（同 seed 逐字元相同：277,103 字元 / 3 次擲骰），
所以這是**既有行為**。`modules/split_message.py` 有能力切，但只有
`RandomPhrasesRewrite.py` 與 `mongo_search` 用它；骰子路徑沒接。

要修的話有兩個方向：把骰子輸出接上 `split_text_by_length`，
或是對骰池加上「總骰數」上限並回報可讀錯誤。
**先量測再決定**——`{100d6 ×8}` 已經 1655 字元，離上限很近，
所以這不只是病態輸入的問題。
