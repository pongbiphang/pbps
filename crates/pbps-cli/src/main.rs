//! `pbps` —— 宣告式資料庫 schema 版控工具。

mod baseline;
mod report;

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};

use pbps_config::Project;
use pbps_dialect::MinimalDialect;
use pbps_diff::{Context, Side};
use pbps_model::{ColumnRef, IdsFile, Intent, TableName};

#[derive(Parser)]
#[command(name = "pbps", version, about = "宣告式資料庫 schema 版控工具")]
struct Cli {
    /// 專案目錄（含 pbps.yml）。預設從目前目錄逐層向上尋找。
    #[arg(long, global = true)]
    project: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 比對宣告檔與基準，產出變更計畫
    Plan {
        /// 基準取自 git 的哪一版
        #[arg(long, default_value = "HEAD")]
        since: String,

        /// 改用指定的狀態快照檔當基準（沒有 git 時使用）
        #[arg(long, conflicts_with = "since")]
        base: Option<PathBuf>,

        /// CI 模式：不修改任何檔案，身份檔未同步即失敗
        #[arg(long)]
        check: bool,

        /// 把變更集寫成 JSON
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// 只檢查宣告檔是否合法，不比對基準
    Validate,

    /// 記錄一次欄位改名
    Rename {
        /// 舊的完整欄位名，如 dbo.customer.customer_name
        from: String,
        /// 新的欄位名（不含表名）
        to: String,
    },

    /// 記錄一次表改名
    RenameTable { from: String, to: String },

    /// 記錄一次欄位刪除
    Drop {
        /// 完整欄位名，如 dbo.customer.national_id
        column: String,
        /// 刪除原因，稽核要求
        #[arg(long)]
        reason: String,
    },

    /// 記錄一次表刪除
    DropTable {
        table: String,
        #[arg(long)]
        reason: String,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("錯誤：{e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let start = match &cli.project {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let project = Project::discover(&start)?;

    match cli.command {
        Command::Plan {
            since,
            base,
            check,
            out,
        } => {
            let source = match base {
                Some(p) => baseline::Source::File(p),
                None if since == "HEAD" => baseline::default_source(&project),
                None => baseline::Source::Git { rev: since },
            };
            cmd_plan(&project, &source, check, out.as_deref())
        }
        Command::Validate => cmd_validate(&project),
        Command::Rename { from, to } => {
            let col: ColumnRef = from.parse()?;
            cmd_intent(
                &project,
                Intent::RenameColumn {
                    table: col.table.clone(),
                    from: col.name,
                    to,
                },
            )
        }
        Command::RenameTable { from, to } => cmd_intent(
            &project,
            Intent::RenameTable {
                from: from.parse()?,
                to: to.parse()?,
            },
        ),
        Command::Drop { column, reason } => cmd_intent(
            &project,
            Intent::DropColumn {
                column: column.parse()?,
                reason,
            },
        ),
        Command::DropTable { table, reason } => cmd_intent(
            &project,
            Intent::DropTable {
                table: table.parse::<TableName>()?,
                reason,
            },
        ),
    }
}

/// 載入宣告檔，把錯誤一次印完。
fn load(project: &Project) -> anyhow::Result<pbps_load::Loaded> {
    let dir = project.schema_dir();
    if !dir.is_dir() {
        bail!("找不到宣告檔目錄 `{}`", dir.display());
    }
    pbps_load::load_schema_dir(&dir).map_err(|errs| {
        for e in &errs {
            eprintln!("{:?}", miette::Report::msg(format!("{e}")));
        }
        anyhow::anyhow!("宣告檔有 {} 個問題", errs.len())
    })
}

fn read_ids(project: &Project) -> anyhow::Result<IdsFile> {
    let path = project.ids_file();
    if !path.exists() {
        return Ok(IdsFile::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("無法讀取身份檔 `{}`", path.display()))?;
    let ids: IdsFile = serde_json::from_str(&text)
        .with_context(|| format!("身份檔 `{}` 格式錯誤", path.display()))?;
    ids.validate()?;
    Ok(ids)
}

fn write_ids(project: &Project, ids: &IdsFile) -> anyhow::Result<()> {
    let path = project.ids_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // 尾端換行，讓檔案在 diff 中不會出現「\ No newline at end of file」
    let json = format!("{}\n", serde_json::to_string_pretty(ids)?);
    std::fs::write(&path, json).with_context(|| format!("無法寫入身份檔 `{}`", path.display()))?;
    Ok(())
}

fn context() -> Context {
    Context {
        operator: operator(),
        today: today(),
    }
}

fn cmd_validate(project: &Project) -> anyhow::Result<()> {
    let loaded = load(project)?;
    println!(
        "宣告檔合法：{} 張表、{} 個欄位。",
        loaded.schema.tables.len(),
        loaded
            .schema
            .tables
            .values()
            .map(|t| t.columns.len())
            .sum::<usize>()
    );
    Ok(())
}

/// 記錄一則意圖：以它重新解析身份，然後寫回身份檔。
fn cmd_intent(project: &Project, intent: Intent) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids(project)?;
    let mut intents = loaded.intents;
    intents.push(intent);

    match pbps_diff::resolve(&loaded.schema, &ids, &intents, &context()) {
        Ok(res) => {
            if res.ids == ids {
                println!("身份檔沒有變化 —— 這則意圖可能已經生效過了。");
                return Ok(());
            }
            write_ids(project, &res.ids)?;
            println!("已更新 {}", project.ids_file().display());
            Ok(())
        }
        Err(blockers) => {
            eprintln!("{}", report::blockers(&blockers));
            bail!("身份無法解析")
        }
    }
}

fn cmd_plan(
    project: &Project,
    source: &baseline::Source,
    check: bool,
    out: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let loaded = load(project)?;
    let ids = read_ids(project)?;

    let res = match pbps_diff::resolve(&loaded.schema, &ids, &loaded.intents, &context()) {
        Ok(r) => r,
        Err(blockers) => {
            eprintln!("{}", report::blockers(&blockers));
            bail!("有無法自動判定的變更，請用上面的指令表達意圖後重試");
        }
    };

    if res.ids != ids {
        if check {
            bail!(
                "身份檔未同步。請在本地執行 `pbps plan` 並將 `{}` 一併 commit。",
                project.ids_file().display()
            );
        }
        write_ids(project, &res.ids)?;
        println!("已更新 {}", project.ids_file().display());
    }

    let base = baseline::load(project, source)?;
    if base.is_empty_fallback {
        eprintln!(
            "警告：基準為空（{}）。所有東西都會被列成新建 —— 這不是對既有資料庫的真實計畫。",
            base.description
        );
    }

    let cs = pbps_diff::diff(
        Side {
            schema: &base.schema,
            ids: &base.ids,
        },
        Side {
            schema: &loaded.schema,
            ids: &res.ids,
        },
        &MinimalDialect,
    )
    .map_err(|errs| {
        for e in &errs {
            eprintln!("  {e}");
        }
        anyhow::anyhow!("有 {} 個變更無法表達", errs.len())
    })?;

    println!("基準：{}", base.description);
    print!("{}", report::plan(&cs));

    if let Some(path) = out {
        std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&cs)?))
            .with_context(|| format!("無法寫入 `{}`", path.display()))?;
        println!("\n已寫入 {}", path.display());
    }
    Ok(())
}

/// 操作者。稽核要回答「誰做的」，git 的設定是最貼近事實的來源。
fn operator() -> String {
    std::process::Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// 今天的日期（UTC，`YYYY-MM-DD`）。
///
/// 不引入日期函式庫 —— 只需要這一個功能，而這個工具會被稽核，依賴樹愈短愈好。
/// 演算法是 Howard Hinnant 的 civil_from_days。
fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // 閏日
        assert_eq!(civil_from_days(20_513), (2026, 3, 1));
        assert_eq!(civil_from_days(20_512), (2026, 2, 28));
    }

    #[test]
    fn today_has_the_expected_shape() {
        let t = today();
        assert_eq!(t.len(), 10, "{t}");
        assert!(t.starts_with("20"), "{t}");
    }
}
