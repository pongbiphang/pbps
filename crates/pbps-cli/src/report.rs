//! 把結果變成人看得懂、而且知道下一步該做什麼的文字。
//!
//! 這裡的原則是：**每一個擋下來的情況都要附上可以直接複製的指令**。非互動
//! 環境下絕不 prompt（CLAUDE.md 約束 6），所以錯誤訊息本身就得是操作指南 ——
//! 否則使用者在 CI log 裡看到「有歧義」卻不知道要打什麼。

use pbps_diff::Blocker;
use pbps_model::{Change, ChangeSet, RiskClass};

pub fn blockers(list: &[Blocker]) -> String {
    let mut out = String::new();
    out.push_str(&format!("有 {} 個變更無法自動判定\n", list.len()));
    for b in list {
        out.push('\n');
        out.push_str(&one_blocker(b));
    }
    out
}

fn one_blocker(b: &Blocker) -> String {
    match b {
        Blocker::AmbiguousColumns {
            table,
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  {table}：{} 消失、{} 是新的\n\n",
                disappeared.join("、"),
                appeared.join("、")
            );
            for from in disappeared {
                for to in appeared {
                    s.push_str(&format!(
                        "    若 {from} 改名為 {to}：pbps rename {table}.{from} {to}\n"
                    ));
                }
                s.push_str(&format!(
                    "    若要刪除 {from}：      pbps drop {table}.{from} --reason \"<原因>\"\n"
                ));
            }
            s
        }
        Blocker::AmbiguousTables {
            disappeared,
            appeared,
        } => {
            let mut s = format!(
                "  表 {} 消失、{} 是新的\n\n",
                join(disappeared),
                join(appeared)
            );
            for from in disappeared {
                for to in appeared {
                    s.push_str(&format!(
                        "    若 {from} 改名為 {to}：pbps rename-table {from} {to}\n"
                    ));
                }
                s.push_str(&format!(
                    "    若要刪除 {from}：      pbps drop-table {from} --reason \"<原因>\"\n"
                ));
            }
            s
        }
        Blocker::DropColumnNeedsReason { column } => format!(
            "  {column} 從宣告檔消失了，但刪除必須留下原因（稽核要回答「為什麼」）\n\n    pbps drop {column} --reason \"<原因>\"\n"
        ),
        Blocker::DropTableNeedsReason { table } => format!(
            "  表 {table} 從宣告檔消失了，但刪除必須留下原因\n\n    pbps drop-table {table} --reason \"<原因>\"\n"
        ),
        Blocker::UnusedIntent { intent } => {
            format!("  這則意圖在宣告檔與身份檔中都對不上，可能是打錯字：\n    {intent:?}\n")
        }
    }
}

fn join<T: std::fmt::Display>(v: &[T]) -> String {
    v.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("、")
}

pub fn plan(cs: &ChangeSet) -> String {
    if cs.is_empty() {
        return "沒有變更。\n".to_owned();
    }

    let mut out = String::new();
    let mut current = None;
    for p in &cs.changes {
        let table = p.change.table().to_string();
        if current.as_deref() != Some(table.as_str()) {
            out.push_str(&format!("\n  {table}\n"));
            current = Some(table);
        }
        let risks = if p.risks.is_empty() {
            String::new()
        } else {
            format!(
                "  [{}]",
                p.risks
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!("    {}{}\n", describe(&p.change), risks));
    }

    let risks = cs.risks();
    if !risks.is_empty() {
        out.push_str(&format!(
            "\n  此計畫含風險變更，套用時需要放行：--allow {}\n",
            risks
                .iter()
                .map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
        if risks.contains(&RiskClass::Destructive) {
            out.push_str("  其中含破壞性變更，會造成資料遺失。\n");
        }
    }
    out
}

fn describe(c: &Change) -> String {
    match c {
        Change::CreateTable { name, table, .. } => {
            format!("+ 建立表 {name}（{} 個欄位）", table.columns.len())
        }
        Change::DropTable { name, .. } => format!("- 刪除表 {name}"),
        Change::RenameTable { from, to, .. } => format!("~ 表改名 {from} → {to}"),
        Change::AddColumn { name, column, .. } => {
            format!("+ 新增欄位 {name} {}", column.ty)
        }
        Change::DropColumn { column, .. } => format!("- 刪除欄位 {}", column.name),
        Change::RenameColumn { from, to, .. } => format!("~ 欄位改名 {from} → {to}"),
        Change::AlterColumnType {
            column, from, to, ..
        } => {
            format!("~ {} 型別 {from} → {to}", column.name)
        }
        Change::AlterColumnNullability {
            column,
            to_nullable,
            ..
        } => format!(
            "~ {} 改為 {}",
            column.name,
            if *to_nullable {
                "可為 NULL"
            } else {
                "NOT NULL"
            }
        ),
        Change::AlterColumnDefault { column, to, .. } => match to {
            Some(v) => format!("~ {} 預設值 → {v}", column.name),
            None => format!("~ {} 移除預設值", column.name),
        },
        Change::SetColumnDeprecated { column, reason, .. } => match reason {
            Some(r) => format!("~ {} 標記棄用：{r}", column.name),
            None => format!("~ {} 取消棄用標記", column.name),
        },
        Change::SetPrimaryKey { to, .. } => match to {
            Some(pk) => format!("~ 主鍵 → ({})", pk.columns.join(", ")),
            None => "- 移除主鍵".to_owned(),
        },
        Change::AddUnique { name, .. } => format!("+ 唯一約束 {name}"),
        Change::DropUnique { name, .. } => format!("- 唯一約束 {name}"),
        Change::AddForeignKey { name, .. } => format!("+ 外鍵 {name}"),
        Change::DropForeignKey { name, .. } => format!("- 外鍵 {name}"),
        Change::AddCheck { name, .. } => format!("+ 檢查約束 {name}"),
        Change::DropCheck { name, .. } => format!("- 檢查約束 {name}"),
        Change::AddIndex { name, .. } => format!("+ 索引 {name}"),
        Change::DropIndex { name, .. } => format!("- 索引 {name}"),
    }
}
