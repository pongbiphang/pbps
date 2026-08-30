# CLAUDE.md

給在這個 repo 上工作的 Claude 的指引。

## 這是什麼

`pbps`（PongBiphang Schema）是宣告式資料庫 schema 版控工具。使用者維護「schema
該長什麼樣」的 YAML，工具算出差異、產生變更腳本、在風險閘門下套用到各環境。

完整設計在 **[docs/SPEC.md](docs/SPEC.md)** —— 動手前先讀，特別是要改資料模型或
新增變更種類時。決策紀錄在 `docs/ADR-*.md`。

競品是 Atlas 與 Skeema（都是宣告式）。差異化不在「宣告式」本身，而在四件事：
rename/drop 的意圖由人給定並記錄在版控中、變更依風險分類、saved plan 用
checksum 釘死、狀態與 ledger 存在資料庫自身。

## 開發環境

**在 WSL 內開發**（Linux 是主要目標）。專案在 `~/pbps`，不要搬到 `/mnt/c`。

不要嘗試 `x86_64-pc-windows-gnu`：rustup 的 self-contained mingw 缺 GNU 組譯器，
`windows-sys`（`clap` 與 `miette` 都會拉）編不過；`gnullvm` 需要外部 llvm-mingw。
細節見 README。

改完一定要跑完這三個，全綠才 commit：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets    # 必須零警告
cargo fmt --all
```

## 架構邊界

```
pbps-model     領域模型。方言無關、不帶 span、序列化目標是 JSON
pbps-config    專案設定（pbps.yml）
pbps-load      YAML → model；唯一可以直接依賴 serde-saphyr 的 crate
pbps-diff      model ↔ ids 比對 → ChangeSet。不產生 SQL
pbps-dialect   方言抽象。純函式；DB 相關的留到 Phase 3 的 DialectDb
pbps-cli       clap、互動 prompt、診斷輸出
```

Phase 2 會加 `pbps-mssql` / `pbps-db`，Phase 4 加 `pbps-pg`。

`spikes/` 被 workspace `exclude`，是獨立的驗證用 crate，不是產品程式碼。

## 不可違反的約束

改動時若發現自己在破壞下面任何一條，先停下來想清楚 —— 這些都是有代價換來的。

**1. `Schema` 必須滿足「兩份語意相同的 schema 一定相等」。**
diff 與 drift 檢查都建立在 `==` 上。所以模型不放 span、不放 `renamed_from`
這類一次性意圖註記（那些由 `pbps-load` 另外回傳），型別比較前要先正規化大小寫。

**2. 容器持有名稱，元素不持有。**
`Table` 沒有 `name`、`Column` 沒有 `name` —— 名稱是父層 map 的 key。這讓
「map key 與內部 name 不一致」寫不出來。需要名稱的函式改成收 `(name, table)`
兩個參數，例如 `Dialect::validate_table`。

**3. SQL 只在方言的 emitter 中出現一次。**
`pbps-diff` 產出的是型別化的 `ChangeSet`，不是字串。風險分類、閘門判斷、
影響分析全部在結構化資料上做。

**4. 風險是資料，不是方法。**
`Change::intrinsic_risks()` 只回答不需要方言知識就能斷定的（drop 是破壞性、
rename 是 rename）。型別窄化需要比較型別，由帶著 `Dialect` 的 differ 算好後
附在 `PlannedChange::risks` 上。模型層不准猜型別風險。

**5. 序列化必須是決定性的。**
身份檔進 git，順序跳動會製造假 diff。集合一律 `BTreeMap` / `BTreeSet`。
唯一例外是 `Table::columns` 用 `IndexMap` 保留宣告順序（影響 CREATE TABLE 的
欄位排列），但它的相等性與順序無關。

**6. 意圖只有 rename 與 drop 需要人給。**
其餘一律自動判定。非互動環境下絕不 prompt —— 直接失敗並印出可複製的指令。

## 格式陷阱（都是實測踩到的）

- **`null` 不能當 YAML 的 key**，那是空值字面量。欄位名用 `nullable`。
- **`no` / `yes` / `on` / `off` 會被解析成布林**。`pbps fmt` 輸出字串純量時
  必須主動加引號，涵蓋布林類、空值類（`null` / `~`）與數字狀字串。
- **版控內一律 LF**（見 `.gitattributes`）。這個工具自己會寫檔，換行符交給
  平台決定會讓「工具擁有檔案格式」的保證在 Windows 上破功。

## 寫作慣例

- 程式碼註解與 commit 訊息用**繁體中文**。
- 註解說明**為什麼**這樣做，不是複述程式碼在做什麼。特別是「為什麼不採用另一種
  看起來更直覺的做法」—— 那是日後最容易被改壞的地方。
- 測試名稱描述被驗證的性質，不是被呼叫的函式
  （`column_order_does_not_affect_equality` 而非 `test_eq`）。
- 每個測試模組要包含「反向案例」：不只驗證正確輸入會過，更要驗證錯誤輸入會被
  擋下。這個工具的失敗模式是靜默地做錯事。
- Commit 用 conventional commits，主旨行中文，內文說明取捨。

## 目前進度

**Phase 0、Phase 1 完成。** 135 個測試、clippy 零警告。

可用的指令：`plan`（含 `--check` / `--since` / `--base` / `--out`）、`validate`、
`fmt`（含 `--check`）、`rename`、`rename-table`、`drop`、`drop-table`。

Phase 1 實作時改掉的原規格決定（SPEC 已同步）：

1. **比對用兩側身份檔以 uid 配對**，不用「名稱 + 本次意圖」。後者在跳版部署下
   會壞：環境落後五版時，當初那則意圖早就不在工作區了。
2. **刪除也要求理由**。原規格說純刪除無歧義、不需意圖，但墓碑要回答稽核的
   「為什麼」，演算法生不出來。
3. **意圖必須冪等**。宣告檔裡的 `renamed_from` 在身份檔更新後仍會留著，
   若判成「對不上的意圖」，使用者會在一次成功的改名後看到莫名的失敗。
4. **`StateSnapshot` 含 `ids`**。狀態與身份必須成對，理由同 1。
5. **IDENTITY 變更明確擋下**，它無法用 ALTER 修改。

**Phase 1 尚未做的**：互動式 prompt（TTY 時的第三個意圖管道）。CLI 指令與
YAML 註記兩條路都能用，非互動是必須支援的路徑，所以這一項不阻塞任何事。

**Phase 2（下一步）**：`pbps-mssql` 的型別正規化、風險判定、SQL emitter、
introspection，以及 `pbps pull`（從既有資料庫反向生成宣告檔 —— 這是採用門檻的
關鍵）。`pbps-dialect::MinimalDialect` 是測試用的替身，Phase 2 要換成真的。
