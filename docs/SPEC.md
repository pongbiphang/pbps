# PongBiphang Schema (`pbps`) — 設計規格

> 狀態：Phase 0 定稿（可實作）
> 語言：Rust
> 定位：宣告式資料庫 schema 版控與部署工具
> 首要方言：SQL Server；次要：PostgreSQL

---

## 1. 定位與範圍

### 1.1 這是什麼

一個**宣告式**的資料庫 schema 管理工具。使用者維護一份「我要的 schema 長什麼樣」的宣告檔，工具負責算出差異、產生變更腳本、在受控的閘門下套用到各環境，並記錄完整稽核軌跡。

對照現有方案：

| 工具 | 模式 | 缺口 |
|---|---|---|
| Flyway / Liquibase | 命令式 | 囉嗦；無法一眼看出 schema 現況；重構困難 |
| Atlas | 宣告式 | rename 靠啟發式偵測，需人工改生成的 migration；HCL 學習曲線；OSS/Pro 功能切分 |
| Skeema | 宣告式 | 僅 MySQL |
| DACPAC | 宣告式 | 綁 SQL Server + Visual Studio 生態；rename 與誤刪風險 |

`pbps` 的差異化不在「宣告式」本身，而在四件事：

1. **rename / drop 的意圖由人明確給定**，記錄在版控中，不靠猜測
2. **變更依風險分類**，危險操作需在指令層明確放行
3. **saved plan + checksum**：review 過的計畫被釘死，apply 時不會多做也不會少做
4. **狀態與稽核 ledger 存在資料庫自身**，天然支援多環境獨立演進

### 1.2 v1 涵蓋範圍

**納入**：table（create / drop / rename）、column、primary key、unique constraint、foreign key、check constraint、index

**延後**：view、stored procedure、function、trigger、權限（GRANT）、資料轉換（backfill）

view / SP 這類「定義即最新版」的物件性質接近 repeatable migration，模型不同，留待後續階段。

### 1.3 明確不做的事

- **資料轉換（backfill）不自動化**。「資料要怎麼搬」是業務決策，無法從結構 diff 推導。需要時由人手動執行 SQL，再走 `pbps baseline` 重設基準。
- **不做跨方言的抽象型別系統**。一份 schema 檔綁定一個方言。「支援多資料庫」指的是工具能操作 MSSQL 與 PostgreSQL，不是同一份檔案能部到兩者。
- **不重播歷史**。宣告式工具沒有 migration history 可重跑；DR 與新環境重建走 `pbps bootstrap` 一次性生成完整 schema，比重播數百支腳本快且可靠。

---

## 2. 核心設計原則

1. **宣告檔只包含期望狀態** —— 你要的欄位，就這些。不含 UID、不含 rename 註記、不含墓碑。
2. **無法推導的資訊才需要人給** —— 只有 rename / drop 的意圖屬於此類，其餘一律自動判定。
3. **意圖一次給定，永久記錄** —— 記在版控的身份檔中，與環境部署進度脫鉤。
4. **人的判斷發生在作者端，不在部署端** —— 打 tag 部署時所有意圖已在 git 中，CI 全自動。
5. **危險操作必須顯性放行** —— 但放行的粒度是「這份被 review 過的計畫」，不是「這個欄位」。
6. **宣告狀態與已驗證狀態分離** —— apply 前必須確認資料庫真實狀態未偏離。

---

## 3. 概念模型

三個持久化產物，職責嚴格分離：

| 產物 | 位置 | 內容 | 誰維護 |
|---|---|---|---|
| `schema/*.yml` | git | 期望狀態：table / column / constraint / index | 人 |
| `schema.ids.json` | git | 身份帳本：uid → 名稱、墓碑紀錄 | 工具 |
| `__pbps_state` 表 | 各環境資料庫 | 該環境已驗證的真實狀態快照 + 稽核 ledger | 工具 |

**為什麼身份檔要進 git**：rename 意圖必須存活到「所有環境都套用完畢」為止。dev 已套用、prod 落後五版是常態，若意圖只活到第一次 apply 就被消化，prod 部署時該資訊已不存在於任何地方。進 git 讓意圖與環境進度脫鉤。

**為什麼狀態要存在資料庫裡**：每個環境的資料庫記得自己的狀態，dev / staging / prod 天然獨立，不需要任何 artifact 傳遞或環境對照策略。

---

## 4. 宣告檔格式

### 4.1 專案設定

```yaml
# pbps.yml
dialect: mssql
schema_dir: schema/
ids_file: schema.ids.json
```

### 4.2 表定義

一張表一個檔案，檔名不具語意（表名由 `table:` 決定）。

```yaml
# schema/dbo.customer.yml
table: dbo.customer
description: 客戶主檔

columns:
  customer_id:
    type: bigint
    nullable: false
    identity: [1, 1]

  full_name:
    type: nvarchar(100)
    nullable: false
    description: 客戶全名

  email:
    type: nvarchar(255)

  balance:
    type: bigint
    nullable: false
    default: 0

  created_at:
    type: datetime2(3)
    nullable: false
    default: SYSUTCDATETIME()

  legacy_code:
    type: varchar(20)
    deprecated: 改用 email 識別

primary_key: [customer_id]

unique:
  uq_customer_email: [email]

foreign_keys:
  fk_customer_region:
    columns: [region_id]
    references: dbo.region(region_id)
    on_delete: no_action

checks:
  ck_customer_balance: balance >= 0

indexes:
  ix_customer_created:
    columns: [created_at]
    include: [full_name]
    where: legacy_code IS NULL
```

### 4.3 格式規則

- **columns 是 map 不是 list**，key 即欄位名。少一層巢狀，且天然禁止重複命名。
- **`nullable` 預設 `true`**，只在需要時寫。欄位名不能用 `null` —— 那是 YAML 的空值字面量，見 [ADR-0001](ADR-0001-yaml-crate.md)。
- **`type` 使用方言原生型別字串**，工具負責正規化（`INT` / `int` / `integer` 視為同一型別）。
- **`deprecated` 只需一句原因**，日期由 git 提供，不用手寫。
- **註解只能寫在 `description` 欄位**。工具擁有檔案格式，`pbps fmt` 會正規化重寫，一般 YAML 註解會遺失。`description` 同時作為資料目錄整合的來源。
- **`pbps fmt` 輸出字串純量時必須加引號**，涵蓋布林類字面量（`true`/`false`/`yes`/`no`/`on`/`off`）、空值類（`null`/`~`）與數字狀字串。否則工具寫出的檔案下次讀取時會變成別的型別。

### 4.4 欄位生命週期

```
(不存在) ──新增──► [Active] ──加 deprecated──► [Deprecated]
                      │                            │
                      │ 從檔案移除                  │ 從檔案移除
                      ▼                            ▼
                   [Dropped] ◄──────────────────────┘
                   （墓碑記在 ids 檔，不在 YAML 中）
```

- **Active**：正常欄位
- **Deprecated**：仍存在於資料庫、仍在 YAML 中（因為它確實是期望狀態的一部分），但不應再有其他屬性變更（見 L009）。可選擇性寫入 extended property 供資料目錄擷取。
- **Dropped**：從 YAML 移除即代表要刪除。工具產生 `DROP COLUMN`，需 `--allow destructive`，並在 ids 檔留下墓碑、在 `__pbps_state` ledger 留下紀錄。

`Deprecated` 不是終態。從 Active 直接 Drop 也允許，只是同樣受 destructive 閘門管制。此設計同時解決「法遵要求實際刪除 PII」與「宣告檔累積殭屍欄位」兩個問題：**檔案裡永遠沒有殭屍，稽核軌跡在 ids 檔與 DB ledger 裡，且可查詢**。

---

## 5. 身份檔（`schema.ids.json`）

### 5.1 格式

```json
{
  "version": 1,
  "tables": {
    "t_a9k2mq": "dbo.customer"
  },
  "columns": {
    "c_k7x2mq": "dbo.customer.customer_id",
    "c_p3n8vd": "dbo.customer.full_name",
    "c_t8h4bn": "dbo.customer.legacy_code"
  },
  "tombstones": {
    "c_v2c9ql": {
      "was": "dbo.customer.national_id",
      "dropped_at": "2026-08-30",
      "reason": "REG-2026-042 PII 刪除要求",
      "operator": "leon"
    }
  }
}
```

只存**無法從 YAML 推導的資訊**：身份對照與墓碑。型別、nullable、index 定義一律不存 —— 那些 YAML 裡就有。

一個 200 欄位的專案就是 200 行 `uid: 名稱`，rename 的 diff 是：

```diff
-    "c_p3n8vd": "dbo.customer.customer_name",
+    "c_p3n8vd": "dbo.customer.full_name",
```

同一個 uid 底下名稱改變 —— 在 MR review 中是無歧義的 rename 訊號。

### 5.2 UID 規則

- 格式：`c_` / `t_` 前綴 + 6 碼 base32 隨機字元
- 全域唯一（不限表內）
- **使用者永不需要輸入或看見**，純為工具內部身份錨點
- 隨機而非流水號：避免兩個分支各自配發相同序號

### 5.3 跨分支衝突

兩個分支各自對同一欄位做不同操作 → 兩邊都改到 ids 檔中同一個 uid 的那一行 → **git merge conflict**，直接擋下。不需要另外設計「標記間參照完整性」的檢查規則。

---

## 6. 意圖表達

只有兩種變更需要人給意圖：**rename** 與 **drop**（當同一張表同時有欄位消失與欄位新增時，兩者無法自動區分）。

三種等價的輸入方式，全部匯流到 ids 檔：

### 6.1 CLI 指令（主要介面）

```bash
pbps rename dbo.customer.customer_name full_name
pbps rename-table dbo.customer dbo.client
pbps drop dbo.customer.national_id --reason "REG-2026-042 PII 刪除要求"
```

完全非互動、可腳本化、**不需要資料庫連線**。

### 6.2 YAML 暫時性註記

```yaml
columns:
  full_name:
    type: nvarchar(100)
    nullable: false
    renamed_from: customer_name    # 暫時性：被 pbps plan 吸收後自動移除
```

`pbps plan` 讀到後將事實寫入 ids 檔，並從 YAML 移除該行。這是**只有編輯器、沒有工具**時的逃生口，不會在檔案中累積。

### 6.3 互動式 prompt

偵測到 TTY 時的便利包裝，實際執行的就是 6.1 的指令。

```
$ pbps plan

  dbo.customer
    ? customer_name 消失了，full_name 是新的
      > 這是改名：customer_name → full_name
        不是，刪除 customer_name 並新增 full_name
```

### 6.4 非互動下的行為

無 TTY 時**絕不 prompt**，直接失敗並給出可複製的指令：

```
$ pbps plan
error: 1 個變更無法自動判定

  dbo.customer: customer_name 消失、full_name 是新的

  若為改名：pbps rename dbo.customer.customer_name full_name
  若為刪除：pbps drop dbo.customer.customer_name --reason "<原因>"
```

---

## 7. 變更分類與風險閘門

### 7.1 自動判定 vs 需要意圖

| 情境 | 需要人的意圖 | 說明 |
|---|---|---|
| 純新增欄位 / 表 / index | 否 | 無欄位消失，必定是 add |
| 型別放寬（INT→BIGINT） | 否 | 安全變更 |
| 型別窄化、加 NOT NULL、加 constraint | 否 | 意圖明確，但屬危險類別 |
| 純刪除，同表無新增 | 否 | 必定是 drop，但屬破壞性類別 |
| **同表同時有消失與新增** | **是** | rename 或 drop+add，無法區分 |

### 7.2 風險類別

| 類別 | 觸發條件 | 風險 |
|---|---|---|
| `rename` | 欄位或表改名 | 依賴物件失效（見 7.4） |
| `destructive` | DROP COLUMN / DROP TABLE / DROP INDEX | 資料遺失 |
| `narrowing` | 型別窄化或不相容轉換 | 截斷、轉換失敗 |
| `not-null` | nullable → NOT NULL 且無 DEFAULT | 既有 NULL 違反 |
| `constraint` | 新增 UNIQUE / FK / CHECK | 既有資料可能不滿足 |

判斷依據是**變更類別本身是否可能失敗**，不讀取資料判斷這次是否剛好安全。資料層面的驗證屬執行期，不在宣告層職責內。

### 7.3 Saved plan + checksum

```
pbps plan  → plan.json（變更清單 + 計算基準的狀態 checksum）
             plan.sql （人類可讀，附在 MR 供 review）

pbps apply --plan plan.json --allow rename,destructive
```

`apply` 先驗證資料庫現況的 checksum 仍等於 plan 計算時的基準，不符即中止（drift 檢查）。

因為變更集合被 checksum 釘死，`--allow` 這種粗粒度旗標是安全的：**放行的就是 MR 裡 review 過的那一份，不多不少**。旗標寫在 CI 設定檔中，是明文且可審查的。

### 7.4 Rename 影響報告

`plan` 偵測到 rename 時，若可連線則查詢依賴並輸出報告：

| 來源（MSSQL） | 查法 | 後果 |
|---|---|---|
| view / SP / function / trigger | `sys.sql_expression_dependencies` | 列出所有參照者 |
| SCHEMABINDING view | 同上 + `is_schema_bound` | **會直接擋住 rename**，須先 DROP |
| 計算欄位 | `sys.computed_columns` | 定義失效 |
| DEFAULT / CHECK 定義 | `sys.check_constraints` | 定義文字含舊名 |
| index / constraint 名稱 | `sys.indexes` | 物件無恙，但名稱可能含舊欄位名（命名漂移） |

方言差異必須被抽象容納：PostgreSQL 儲存解析後的依賴，`RENAME COLUMN` 會自動更新 view；SQL Server 儲存定義文字，`sp_rename` **不會**更新。這正是 `rename_impact` 屬於 `Dialect` trait 的原因。

資料庫外的影響（應用程式、報表、下游 ELT）工具看不到，輸出 checklist 附於 MR 供人簽核。

---

## 8. 環境狀態與 drift

### 8.1 `__pbps_state`

```sql
CREATE TABLE dbo.__pbps_state (
    id            BIGINT IDENTITY PRIMARY KEY,
    applied_at    DATETIME2(3)   NOT NULL,
    kind          VARCHAR(16)    NOT NULL,   -- apply | baseline | bootstrap
    git_sha       VARCHAR(40)    NULL,
    plan_checksum CHAR(64)       NULL,
    state_json    NVARCHAR(MAX)  NOT NULL,   -- 整份 schema 快照
    operator      NVARCHAR(128)  NOT NULL,
    reason        NVARCHAR(1000) NULL
);

CREATE TABLE dbo.__pbps_lock (
    id         INT PRIMARY KEY CHECK (id = 1),
    locked_by  NVARCHAR(256) NOT NULL,
    locked_at  DATETIME2(3)  NOT NULL
);
```

存**整份快照**而非增量或 checksum：drift 檢查可完整比對、可作為備援、可回答「三個月前這張表長什麼樣」。搭配 `pbps state prune --keep 50` 清理。

`__pbps_lock` 防止兩條 pipeline 同時 apply。

### 8.2 Drift 檢查

`apply` 之前：實查 `sys.columns` / `INFORMATION_SCHEMA` → 與最新一列 `state_json` 比對 → 不符即中止，要求人工校準。

這攔截的是「有人手動 SSH 上去改了 schema」的情形。

### 8.3 Drift 之後的兩條路

| 路徑 | 指令 | 適用 |
|---|---|---|
| 接受現實：把手動變更回寫進宣告檔 | `pbps pull --table X` 後人工合併 | 手動變更是對的、應保留 |
| 回復宣告：把資料庫改回宣告狀態 | 修正後正常 `plan` / `apply` | 手動變更是誤操作 |
| 重設基準：不追究差異，以現況為新起點 | `pbps baseline --reason ... --operator ...` | DBA 已依特批手動處理完畢 |

`baseline` 會實查資料庫寫入新的 `state_json`，並在 ledger 記錄 reason / operator / 時間戳。

---

## 9. CLI 指令集

### 9.1 不需資料庫連線

| 指令 | 用途 |
|---|---|
| `pbps plan` | 比對 YAML ↔ ids 檔，產出 plan.json / plan.sql，解決意圖歧義 |
| `pbps plan --check` | CI 模式：僅在「意圖缺失」時失敗，不 prompt |
| `pbps fmt` | 正規化宣告檔格式 |
| `pbps rename` / `rename-table` / `drop` | 記錄意圖到 ids 檔 |
| `pbps validate` | 靜態檢查（型別合法性、FK 目標存在、命名規則） |

`plan` 不需要資料庫，是刻意的設計：**正式環境不可直連時，開發者仍能在本地完整作業**。

### 9.2 需要資料庫連線

| 指令 | 用途 |
|---|---|
| `pbps pull` | 從現有資料庫反向生成 YAML 宣告檔（新使用者的第一步） |
| `pbps verify` | drift 檢查：實查 vs `__pbps_state` |
| `pbps apply --plan plan.json --allow ...` | 套用計畫 |
| `pbps snapshot` | 實查資料庫寫入新的 `__pbps_state` |
| `pbps baseline --reason --operator` | 重設狀態基準 |
| `pbps bootstrap` | 從宣告檔生成完整 CREATE 腳本（DR / 新環境） |
| `pbps state prune --keep N` | 清理歷史快照 |

`pbps pull` 是採用門檻的關鍵：任何新使用者的第一步都是「我已經有一個資料庫了」。沒有這個指令，導入成本等同手抄兩百張表。

---

## 10. CI/CD 流程

```yaml
stages: [check, plan, verify, apply, record]

check:
  script:
    - pbps plan --check          # 意圖缺失才失敗，其餘全自動
    - pbps validate
    - pbps fmt --check

plan:
  script:
    - pbps plan --out plan.json --sql plan.sql
  artifacts:
    paths: [plan.json, plan.sql]   # plan.sql 附在 MR 供 review

verify:
  script:
    - pbps verify --db "$PROD_CONN"
  only: [/^prod-v.*$/]

apply:
  script:
    - pbps apply --db "$PROD_CONN" --plan plan.json --allow rename,narrowing
  when: manual
  only: [/^prod-v.*$/]

record:
  script:
    - pbps snapshot --db "$PROD_CONN"
  only: [/^prod-v.*$/]
```

**部署階段完全不需要人的判斷。** rename / drop 的意圖在開發者寫 MR 時就已解決並進入 git，打 tag 時 CI 只是照著執行。`when: manual` 是核可閘門，不是決策點。

---

## 11. 架構

### 11.1 Crate 切分

```
pbps/
  crates/
    pbps-model/     領域模型：Schema, Table, Column, ColumnType, Uid,
                    ChangeSet, RiskClass；ids 檔與 state 的序列化
    pbps-config/    pbps.yml 專案設定
    pbps-load/      YAML 載入 + span 錯誤診斷；fmt 正規化輸出
    pbps-diff/      YAML ↔ ids 比對 → ChangeSet（純資料，不產生 SQL）
    pbps-dialect/   Dialect trait 定義 + 共用工具
    pbps-mssql/     MSSQL：型別正規化、SQL 生成、introspection、依賴查詢
    pbps-pg/        PostgreSQL（Phase 4）
    pbps-db/        連線抽象、__pbps_state 讀寫、lock
    pbps-cli/       clap、互動 prompt、診斷輸出
```

**分層要點**：`diff` 產出的是型別化的 `ChangeSet`，不是 SQL 字串。風險分類、閘門判斷、影響分析全部在結構化資料上進行；SQL 只在 dialect crate 的 emitter 中出現一次。

### 11.2 `Dialect` trait

```rust
pub trait Dialect {
    fn name(&self) -> &'static str;

    /// 型別字串正規化：INT / int / integer → 同一個 ColumnType
    fn parse_type(&self, s: &str) -> Result<ColumnType>;
    fn render_type(&self, t: &ColumnType) -> String;

    /// 型別變更的風險判定（放寬 / 窄化 / 不相容）
    fn type_change_risk(&self, from: &ColumnType, to: &ColumnType) -> Risk;

    fn quote_ident(&self, s: &str) -> String;

    /// ChangeSet → 可執行語句
    fn emit(&self, change: &Change) -> Result<Vec<Statement>>;

    fn introspect(&self, conn: &mut Conn) -> Result<Schema>;

    /// rename 的依賴影響（MSSQL 需要，PG 幾乎為空）
    fn rename_impact(&self, conn: &mut Conn, target: &RenameTarget)
        -> Result<ImpactReport>;
}
```

`pbps-model` / `pbps-diff` / `pbps-load` 完全方言無關。新增一個資料庫是有明確邊界的工作。

### 11.3 主要依賴

| 用途 | crate | 備註 |
|---|---|---|
| SQL Server | `tiberius` | 純 Rust，**不需安裝 ODBC driver** —— 對 air-gapped 環境是決定性的 |
| PostgreSQL | `tokio-postgres` | Phase 4 |
| 非同步 | `tokio` + `tokio-util`（tiberius compat） | |
| CLI | `clap`（derive） | |
| 診斷 | `miette` | 帶 source span 的錯誤訊息；Phase 1 的產品體驗核心 |
| 錯誤 | `thiserror`（lib）/ `anyhow`（bin） | |
| 序列化 | `serde` + `serde_json` | ids/state 需正規化輸出：`BTreeMap`、固定排序 |
| YAML | `serde-saphyr` | 已定案，見 [ADR-0001](ADR-0001-yaml-crate.md)。錯誤帶 `offset + len` span、自帶原始碼片段、內建重複 key 偵測。MSRV 因此為 1.89 |
| 互動 prompt | `dialoguer` 或 `inquire` | |
| 測試 | `insta` | snapshot 測 AST、診斷輸出、生成的 SQL |

**明確不用**：`sqlx`（compile-time 檢查對動態 DDL 無意義）、`diesel`、任何 SQL parser（改用 YAML 後不需要）。

### 11.4 散布

單一靜態執行檔。Windows runner 用 `x86_64-pc-windows-msvc`；Linux runner 用 `x86_64-unknown-linux-musl` 產出全靜態檔案。`tiberius` 為純 Rust 實作，兩者都能做到「複製一個檔案就能跑」，不需要在 air-gapped 主機上安裝任何執行環境。

### 11.5 測試策略

這類工具容易寫出「測試全過但會掉資料」，因此四層不變式必須被機器驗證：

1. **格式 round-trip**：`load(fmt(schema)) == schema`
2. **Bootstrap 一致性**：宣告檔 → `bootstrap` 建到空庫 → `introspect` → 應等於宣告狀態。保證「宣告 ≡ 資料庫實況」
3. **Migration convergence**：資料庫在狀態 A → 套用 `plan(A→B)` → `introspect` → 應等於狀態 B。**最重要的一條**，跑在 docker 的真實 SQL Server 上
4. **診斷 snapshot**：所有錯誤訊息以 `insta` 固定

---

## 12. 階段規劃

| 階段 | 內容 | 交付價值 |
|---|---|---|
| **Phase 0** | workspace 骨架、`pbps-model` 資料模型、YAML/ids 格式定稿、`Dialect` trait 介面、YAML crate 的 span 能力驗證 | 所有東西的地基，改起來最貴 |
| **Phase 1** | `load` / `fmt` / `diff` / ids 檔 / 意圖三管道 / `plan` / `plan --check` / `validate` | 純檔案層、零風險。已可產出 plan.sql 供人工執行 |
| **Phase 2** | MSSQL emitter + introspection + **`pbps pull`** | 反向生成解決導入門檻，是採用的關鍵 |
| **Phase 3** | `__pbps_state` / lock / `verify` / `apply` / `--allow` 閘門 / rename 影響報告 / `snapshot` / `baseline` / `bootstrap` | 完整產品 |
| **Phase 4** | PostgreSQL 方言 | 抽象正確性的試金石。Phase 0 設計時即以 PG 為假想案例檢驗 |
| **Phase 5** | view / SP 的 repeatable 模式、extended property 與資料目錄整合、更多方言 | |

Phase 0 設計 `Dialect` trait 時**必須同時考慮 PostgreSQL**（即使不實作）。若 Phase 4 逼你大改 `pbps-model`，代表 Phase 0 的抽象抓錯了。

---

## 13. 已知的未解問題

1. **`serde-saphyr` 的供應鏈風險** —— 已選定（[ADR-0001](ADR-0001-yaml-crate.md)），但它是年輕的單一維護者 crate。緩解方式是架構上已有的隔離：只有 `pbps-load` 直接依賴它。需持續觀察維護狀況。

2. **大表 ALTER 的執行策略** —— ONLINE 選項、分批、離峰排程屬執行期決策，diff 推不出來。可能方向：表層級的 `strategy:` 標注，或允許 plan.sql 在 MR 階段人工編修後再 apply。

3. **應用程式與資料庫的部署時序協調** —— zero-downtime 常需 schema 變更與應用版本錯開。工具本身不管這個，但 `--allow` 與 saved plan 讓「何時套用哪個版本」可控。expand → 雙寫 → backfill → contract 這類多階段流程需跨多次部署，目前無工具支援。

4. **`baseline` 的權限控管** —— 誰能執行、如何與 GitLab approval 串接，待設計。

5. **資料轉換（backfill）** —— 目前明確不做（見 1.3）。若日後累積出重複 pattern，再以真實案例為規格設計 hook 機制。

6. **view / SP 的處理模式** —— 這類物件「定義即最新版」，性質接近 repeatable migration，與欄位的身份追蹤模型不同，需要獨立設計。

7. **權限（GRANT）是否納入** —— 宣告式權限管理有價值，但與 schema 變更的風險模型不同，且各方言差異大。

---

## 附錄 A：命名查證

`pbps` 於 2026-08 查證結果：

- crates.io：**無同名 crate**（`pbm` 亦無，但 `pbs` 已被 OpenPBS FFI 佔用）
- 無任何同名 CLI 執行檔，無 PATH 衝突
- 唯一同名為 PBPS（Performance Based Prevention System，美國藥物濫用防治通報系統），不同領域、非 CLI，不構成實務困擾

被排除的候選：

- `pbm` —— 撞 Percona Backup for MongoDB（**同為資料庫工具，最嚴重**）、Netpbm 圖像格式、Petabridge.Cmd
- `pbs` —— crates.io 已被佔用；撞 Portable Batch System（HPC 排程器）、PYBOSSA CLI

**建議儘早於 crates.io 發佈 `0.0.0` 佔位版鎖定名稱。**
