# ADR-0001：YAML crate 選用 serde-saphyr

- 狀態：已決定
- 日期：2026-08-30
- 相關：docs/SPEC.md §11.3、§13.1
- 驗證程式：`spikes/yaml-span/`（定案後可移除）

## 背景

`pbps` 的宣告檔是 YAML，而 Linter 的產品價值直接取決於錯誤訊息能不能指到**出錯的那一行**。原本規格把 YAML crate 列為 Phase 0 的第一個待決事項，選擇標準寫明：「錯誤必須帶行號 span」。

`serde_yaml` 已停止維護（0.9.34+deprecated），必須另選。

## 驗收條件

| 編號 | 情境 | 為什麼重要 |
|---|---|---|
| A | 語法錯誤 | 基本要求 |
| B | 型別錯誤 | 必須指到**值**的位置，不是文件開頭 |
| C | 未知欄位 | 拼錯欄位名是最常見的使用者錯誤 |
| D | 重複 key | 同一張表出現兩個同名欄位，**靜默接受會導致靜默資料遺失** |
| E | 語意錯誤的 span | 文件合法但值在業務上無效（未知型別、uid 重複）。**Linter 的核心需求** |
| F | Norway problem | YAML 1.1 把 `no` / `yes` / `on` / `off` 當布林 |

## 實測結果

| 驗收 | `serde-saphyr` 1.2 | `marked-yaml` 0.8 |
|---|---|---|
| A | `line 5 column 4` + 原始碼片段與 caret | `5:12` |
| B | `line 8 column 15: invalid boolean` —— 指到值 | `Value was not a boolean` —— **無位置** |
| C | `line 5 column 5: unknown field \`nullabel\`, expected one of type, nullable` | `Unknown field ...` —— **無位置** |
| D | `line 5 column 3: duplicate mapping key: email`，且可用 `DuplicateKeyPolicy` 設定 | **靜默接受，最後一個獲勝** |
| E | `line 7, column 11, span { offset: 102, len: 9 }` | `line 7, column 11`，但 `end: None` |
| F | key `no` 可正常作為欄位名；`nullable: no` 被解析為 `false` | 未測 |

`serde_yaml_ng` / `serde_yaml_neo` 等 `serde_yaml` 分支未進入實測：它們沿用 `serde_yaml` 的架構，沒有 `Spanned<T>`，E 條先天不滿足。

## 決定

**採用 `serde-saphyr`。**

決定性的兩點：

1. **E 給出 `offset + len` 的完整 span**，可直接建成 `miette::SourceSpan`。`marked-yaml` 只給起點，畫不出底線範圍。
2. **D 內建重複 key 偵測**。同一張表兩個同名欄位若被靜默吞掉，宣告檔與資料庫會產生無聲的偏差 —— 這正是本工具存在的意義所在，不能靠自己另外做一層檢查補救。

附帶收穫：A–D 的錯誤已自帶原始碼片段與 caret，診斷品質接近 miette 手工輸出。

## 代價與緩解

- **`serde-saphyr` 是年輕的單一維護者 crate（1.2.0）。** 供應鏈風險真實存在。緩解方式已存在於架構中：所有 YAML 存取封閉在 `pbps-load` 內，其餘 crate 不直接依賴它，替換是有界的工作。
- **MSRV 被抬到 1.89**（`serde-saphyr` 的要求），workspace 的 `rust-version` 隨之調整。

## 連帶的格式修正

Spike 發現規格 §4.1 的欄位名 `null` **是無效的 YAML 格式**：

```yaml
columns:
  customer_id:
    type: bigint
    null: false     # ← `null` 是 YAML 空值字面量，不是字串 "null"
```

`serde-saphyr` 正確地報 `cannot deserialize null into string`。`marked-yaml` 反而寬鬆地接受了 —— 這種「有的 parser 過、有的不過」正是最該避免的格式。

**改為 `nullable`**，語意也更貼近 SQL 詞彙：

```yaml
columns:
  customer_id:
    type: bigint
    nullable: false
```

## 連帶的 Phase 1 需求

F 顯示 `no` 會被解析成布林。我們的字串欄位（`description`、`deprecated` 的原因、`type`）若剛好收到裸 `no` / `yes` / `on` / `off` / `~` 或看起來像數字的值，會變成型別錯誤 —— 是大聲失敗而非靜默損壞，可以接受。

但 **`pbps fmt` 在輸出字串純量時必須主動加引號**，涵蓋：布林類字面量（`true`/`false`/`yes`/`no`/`on`/`off`/`y`/`n` 及其大小寫變體）、空值類（`null`/`~`）、以及任何可被解析為數字的字串。否則工具自己寫出來的檔案會在下次讀取時變成別的型別。
