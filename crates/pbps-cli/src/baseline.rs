//! 「目前狀態」從哪裡來。
//!
//! 屬性變更（型別、nullable…）需要一個基準才算得出來，而身份檔刻意只存
//! uid → 名稱，推導不出屬性。基準有三個來源，優先序如下：
//!
//! 1. `--base <檔案>`：明確指定的狀態快照。**不自動讀也不自動寫** ——
//!    它是逃生口，給沒有 git 的環境（export 式簽出、air-gapped 壓縮檔），
//!    而不是第二套要維護的產物。
//! 2. git 中的某一版（預設 `HEAD`）。基準是什麼一目了然，
//!    `--since v1.2.0` 就是「從那個版本以來」。
//! 3. 空基準。所有東西都會被列成新建 —— 這在第一次執行時是對的，
//!    但被誤當成真實計畫會很危險，因此一定要大聲說出來。
//!
//! 無論哪一種，離線算出的都是**預覽**。真正要套用到某個環境的計畫，必須以
//! 該環境資料庫的實查狀態為基準（Phase 3）。

use std::path::{Path, PathBuf};
use std::process::Command;

use pbps_config::Project;
use pbps_model::Schema;

#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    Git { rev: String },
    Empty,
}

/// 基準狀態，連同一句給人看的來源說明。
pub struct Baseline {
    pub schema: Schema,
    pub description: String,
    /// 空基準要提醒使用者，否則「全部都是新建」會被誤讀成真實計畫。
    pub is_empty_fallback: bool,
}

pub fn load(project: &Project, source: &Source) -> anyhow::Result<Baseline> {
    match source {
        Source::File(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("無法讀取基準檔 `{}`：{e}", path.display()))?;
            let schema: Schema = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("基準檔 `{}` 格式錯誤：{e}", path.display()))?;
            Ok(Baseline {
                schema,
                description: format!("基準檔 {}", path.display()),
                is_empty_fallback: false,
            })
        }
        Source::Git { rev } => load_from_git(project, rev),
        Source::Empty => Ok(Baseline {
            schema: Schema::default(),
            description: "空基準".into(),
            is_empty_fallback: true,
        }),
    }
}

/// 決定預設來源：在 git repo 內就用 `HEAD`，否則退回空基準。
pub fn default_source(project: &Project) -> Source {
    if in_git_repo(&project.root) {
        Source::Git {
            rev: "HEAD".to_owned(),
        }
    } else {
        Source::Empty
    }
}

fn in_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--show-toplevel"]).is_ok()
}

fn load_from_git(project: &Project, rev: &str) -> anyhow::Result<Baseline> {
    let root = &project.root;
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])
        .map_err(|e| anyhow::anyhow!("這裡不是 git 工作區，請改用 --base 指定基準檔：{e}"))?;
    let toplevel = PathBuf::from(toplevel.trim());

    // git 的路徑以 repo 根目錄為基準，宣告檔目錄則相對於專案根目錄。
    let schema_dir = project.schema_dir();
    let abs = std::fs::canonicalize(&schema_dir).unwrap_or(schema_dir.clone());
    let rel = abs.strip_prefix(&toplevel).unwrap_or(&abs);
    let rel = rel.to_string_lossy().replace('\\', "/");

    let listing = git(root, &["ls-tree", "-r", "--name-only", rev, "--", &rel]).map_err(|e| {
        anyhow::anyhow!("無法讀取 `{rev}` 的 `{rel}`：{e}")
    })?;

    let mut schema = Schema::default();
    let mut count = 0usize;
    for path in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if !(path.ends_with(".yml") || path.ends_with(".yaml")) {
            continue;
        }
        let text = git(root, &["show", &format!("{rev}:{path}")])?;
        match pbps_load::load_table_str(Path::new(path), &text) {
            Ok(t) => {
                schema.tables.insert(t.name, t.table);
                count += 1;
            }
            Err(errs) => {
                // 基準是歷史版本，它壞掉不該讓現在的工作停擺，但要說出來。
                anyhow::bail!(
                    "`{rev}` 中的 `{path}` 無法解析（基準版本本身有問題）：{}",
                    errs.first().map(ToString::to_string).unwrap_or_default()
                );
            }
        }
    }

    Ok(Baseline {
        schema,
        description: format!("git {rev}（{count} 張表）"),
        is_empty_fallback: count == 0,
    })
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("無法執行 git：{e}"))?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
