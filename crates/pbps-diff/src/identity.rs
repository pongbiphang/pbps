//! 身份解析：把宣告檔的名稱對應回 UID，並找出哪些變化需要人來裁決。
//!
//! # 核心問題
//!
//! 身份檔記的是「上次已知的 uid → 名稱」。宣告檔記的是「現在要什麼」。兩邊
//! 一比就會出現三種情形：名稱兩邊都在（同一個東西）、只在宣告檔（新的）、
//! 只在身份檔（消失了）。
//!
//! 麻煩的是最後兩種**同時**發生：一張表裡既有欄位消失、又有欄位出現時，
//! 結構上無法區分那是改名還是刪除加新增。這個資訊只存在於當事人腦中，
//! 因此必須由人給（[`Intent`]），演算法不准猜 —— 猜錯的代價是掉資料。
//!
//! # 為什麼刪除也要理由
//!
//! 「純刪除、同表無新增」在操作上確實沒有歧義。但墓碑要回答稽核的
//! 「誰、何時、為什麼刪的」，而理由沒有任何演算法生得出來。因此刪除同樣
//! 需要一則意圖 —— 差別在於它要的不是「是不是改名」，而是「為什麼」。

use std::collections::{BTreeMap, BTreeSet};

use pbps_model::{ColumnRef, IdsFile, Intent, Schema, TableName, Tombstone, Uid, UidKind};

/// 無法自動判定、必須由人處理的情況。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// 同一張表同時有欄位消失與新增。
    AmbiguousColumns {
        table: TableName,
        disappeared: Vec<String>,
        appeared: Vec<String>,
    },
    /// 同時有表消失與新增。
    AmbiguousTables {
        disappeared: Vec<TableName>,
        appeared: Vec<TableName>,
    },
    /// 欄位從宣告檔消失，但沒有提供刪除理由。
    DropColumnNeedsReason { column: ColumnRef },
    /// 表從宣告檔消失，但沒有提供刪除理由。
    DropTableNeedsReason { table: TableName },
    /// 給了意圖，但宣告檔與身份檔裡都對不上 —— 幾乎一定是打錯字。
    ///
    /// 靜默忽略會讓使用者接著看到一個他不理解的歧義錯誤。
    UnusedIntent { intent: Intent },
}

/// 產生墓碑所需、但無法從檔案推導的資訊。
#[derive(Debug, Clone)]
pub struct Context {
    pub operator: String,
    /// `YYYY-MM-DD`。由呼叫端提供，讓這一層維持純函式、可測試。
    pub today: String,
}

/// 身份解析的結果 —— 只有身份層級的事實，不含屬性變更。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolution {
    /// 套用本次變化後的身份檔。
    pub ids: IdsFile,
    pub created_tables: Vec<(Uid, TableName)>,
    pub dropped_tables: Vec<(Uid, TableName)>,
    /// `(uid, 舊名, 新名)`
    pub renamed_tables: Vec<(Uid, TableName, TableName)>,
    pub added_columns: Vec<(Uid, ColumnRef)>,
    pub dropped_columns: Vec<(Uid, ColumnRef)>,
    pub renamed_columns: Vec<(Uid, ColumnRef, ColumnRef)>,
}

pub fn resolve(
    declared: &Schema,
    ids: &IdsFile,
    intents: &[Intent],
    ctx: &Context,
) -> Result<Resolution, Vec<Blocker>> {
    let mut r = Resolution {
        ids: ids.clone(),
        ..Default::default()
    };
    let mut blockers = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();

    resolve_tables(declared, intents, ctx, &mut r, &mut blockers, &mut used);
    resolve_columns(declared, intents, ctx, &mut r, &mut blockers, &mut used);

    for (i, intent) in intents.iter().enumerate() {
        if !used.contains(&i) && !already_satisfied(intent, &r.ids) {
            blockers.push(Blocker::UnusedIntent {
                intent: intent.clone(),
            });
        }
    }

    if blockers.is_empty() {
        sort_resolution(&mut r);
        Ok(r)
    } else {
        Err(blockers)
    }
}

/// 這則意圖是不是「已經生效了」。
///
/// 意圖必須是冪等的。宣告檔中的 `renamed_from` 註記在身份檔更新後仍會留在
/// 檔案裡（要等 `pbps fmt` 才清掉），若把它當成對不上的意圖報錯，使用者會在
/// 一次成功的改名之後看到一個莫名其妙的失敗。
///
/// 判斷方式是看世界是否已經處於這則意圖想要的樣子：改名的目標名稱已存在且
/// 來源名稱已不存在、或刪除的對象已經不在身份檔中。
fn already_satisfied(intent: &Intent, ids: &IdsFile) -> bool {
    let has_column = |c: &ColumnRef| ids.columns.values().any(|v| v == c);
    let has_table = |t: &TableName| ids.tables.values().any(|v| v == t);

    match intent {
        Intent::RenameTable { from, to } => has_table(to) && !has_table(from),
        Intent::RenameColumn { table, from, to } => {
            has_column(&table.column(to)) && !has_column(&table.column(from))
        }
        Intent::DropTable { table, .. } => !has_table(table),
        Intent::DropColumn { column, .. } => !has_column(column),
    }
}

fn resolve_tables(
    declared: &Schema,
    intents: &[Intent],
    ctx: &Context,
    r: &mut Resolution,
    blockers: &mut Vec<Blocker>,
    used: &mut BTreeSet<usize>,
) {
    let declared_names: BTreeSet<&TableName> = declared.tables.keys().collect();
    // 取得擁有權的副本：底下要邊查邊改 `r.ids`。
    let known: BTreeMap<TableName, Uid> = r
        .ids
        .tables
        .iter()
        .map(|(u, n)| (n.clone(), u.clone()))
        .collect();

    let mut appeared: BTreeSet<TableName> = declared_names
        .iter()
        .filter(|n| !known.contains_key(**n))
        .map(|n| (*n).clone())
        .collect();
    let mut disappeared: BTreeSet<TableName> = known
        .keys()
        .filter(|n| !declared_names.contains(n))
        .cloned()
        .collect();

    // 改名優先於刪除：同一張表若兩種意圖都給了，改名的語意更具體。
    for (i, intent) in intents.iter().enumerate() {
        if let Intent::RenameTable { from, to } = intent
            && disappeared.remove(from)
            && appeared.remove(to)
        {
            let uid = known[from].clone();
            rename_table_in_ids(&mut r.ids, &uid, from, to);
            r.renamed_tables.push((uid, from.clone(), to.clone()));
            used.insert(i);
        }
    }

    for (i, intent) in intents.iter().enumerate() {
        if let Intent::DropTable { table, reason } = intent
            && disappeared.remove(table)
        {
            let uid = known[table].clone();
            drop_table_in_ids(&mut r.ids, &uid, table, reason, ctx);
            r.dropped_tables.push((uid, table.clone()));
            used.insert(i);
        }
    }

    if !appeared.is_empty() && !disappeared.is_empty() {
        blockers.push(Blocker::AmbiguousTables {
            disappeared: disappeared.into_iter().collect(),
            appeared: appeared.into_iter().collect(),
        });
        return;
    }

    for table in disappeared {
        blockers.push(Blocker::DropTableNeedsReason { table });
    }

    for name in appeared {
        let uid = fresh_uid(&r.ids, UidKind::Table);
        r.ids.tables.insert(uid.clone(), name.clone());
        r.created_tables.push((uid, name));
    }
}

fn resolve_columns(
    declared: &Schema,
    intents: &[Intent],
    ctx: &Context,
    r: &mut Resolution,
    blockers: &mut Vec<Blocker>,
    used: &mut BTreeSet<usize>,
) {
    // 只處理宣告檔中仍存在的表。新建表的欄位一律是新增；已刪除的表，
    // 其欄位的墓碑在 drop_table_in_ids 中一併處理。
    for (table_name, table) in &declared.tables {
        let declared_cols: BTreeSet<&String> = table.columns.keys().collect();
        let known: BTreeMap<String, Uid> = r
            .ids
            .columns
            .iter()
            .filter(|(_, c)| &c.table == table_name)
            .map(|(u, c)| (c.name.clone(), u.clone()))
            .collect();

        let mut appeared: BTreeSet<String> = declared_cols
            .iter()
            .filter(|n| !known.contains_key(**n))
            .map(|n| (*n).clone())
            .collect();
        let mut disappeared: BTreeSet<String> = known
            .keys()
            .filter(|n| !declared_cols.contains(n))
            .cloned()
            .collect();

        for (i, intent) in intents.iter().enumerate() {
            if let Intent::RenameColumn { table, from, to } = intent
                && table == table_name
                && disappeared.remove(from)
                && appeared.remove(to)
            {
                let uid = known[from].clone();
                let old = table_name.column(from);
                let new = table_name.column(to);
                r.ids.columns.insert(uid.clone(), new.clone());
                r.renamed_columns.push((uid, old, new));
                used.insert(i);
            }
        }

        for (i, intent) in intents.iter().enumerate() {
            if let Intent::DropColumn { column, reason } = intent
                && &column.table == table_name
                && disappeared.remove(&column.name)
            {
                let uid = known[&column.name].clone();
                r.ids.columns.remove(&uid);
                r.ids.tombstones.insert(
                    uid.clone(),
                    Tombstone {
                        was: column.to_string(),
                        dropped_at: ctx.today.clone(),
                        reason: reason.clone(),
                        operator: ctx.operator.clone(),
                    },
                );
                r.dropped_columns.push((uid, column.clone()));
                used.insert(i);
            }
        }

        if !appeared.is_empty() && !disappeared.is_empty() {
            blockers.push(Blocker::AmbiguousColumns {
                table: table_name.clone(),
                disappeared: disappeared.into_iter().collect(),
                appeared: appeared.into_iter().collect(),
            });
            continue;
        }

        for name in disappeared {
            blockers.push(Blocker::DropColumnNeedsReason {
                column: table_name.column(name),
            });
        }

        for name in appeared {
            let uid = fresh_uid(&r.ids, UidKind::Column);
            let col = table_name.column(name);
            r.ids.columns.insert(uid.clone(), col.clone());
            r.added_columns.push((uid, col));
        }
    }
}

/// 表改名時，它底下所有欄位的參照也要一起改 —— 欄位的身份沒變，但它們的
/// 限定名稱包含表名。漏掉這一步，下一次 diff 會把整張表的欄位看成全新的。
fn rename_table_in_ids(ids: &mut IdsFile, uid: &Uid, from: &TableName, to: &TableName) {
    ids.tables.insert(uid.clone(), to.clone());
    let moved: Vec<(Uid, ColumnRef)> = ids
        .columns
        .iter()
        .filter(|(_, c)| &c.table == from)
        .map(|(u, c)| (u.clone(), to.column(&c.name)))
        .collect();
    for (u, c) in moved {
        ids.columns.insert(u, c);
    }
}

fn drop_table_in_ids(ids: &mut IdsFile, uid: &Uid, table: &TableName, reason: &str, ctx: &Context) {
    let make = |was: String| Tombstone {
        was,
        dropped_at: ctx.today.clone(),
        reason: reason.to_owned(),
        operator: ctx.operator.clone(),
    };

    ids.tables.remove(uid);
    ids.tombstones.insert(uid.clone(), make(table.to_string()));

    let cols: Vec<(Uid, ColumnRef)> = ids
        .columns
        .iter()
        .filter(|(_, c)| &c.table == table)
        .map(|(u, c)| (u.clone(), c.clone()))
        .collect();
    for (u, c) in cols {
        ids.columns.remove(&u);
        ids.tombstones.insert(u, make(c.to_string()));
    }
}

/// 配一個不與現有身份衝突的 UID。
///
/// 碰撞極罕見，但「靜默重用同一個身份」會直接造成錯誤的改名判定，
/// 所以寧可多檢查一次。
fn fresh_uid(ids: &IdsFile, kind: UidKind) -> Uid {
    loop {
        let u = Uid::generate(kind);
        if !ids.tables.contains_key(&u)
            && !ids.columns.contains_key(&u)
            && !ids.tombstones.contains_key(&u)
        {
            return u;
        }
    }
}

/// 輸出順序必須穩定，否則同樣的輸入會產生不同的計畫與診斷順序。
fn sort_resolution(r: &mut Resolution) {
    r.created_tables.sort();
    r.dropped_tables.sort();
    r.renamed_tables.sort();
    r.added_columns.sort();
    r.dropped_columns.sort();
    r.renamed_columns.sort();
}
