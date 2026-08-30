//! 專案設定（`pbps.yml`）與路徑解析。
//!
//! 設定檔的位置定義了「專案根目錄」，其餘所有相對路徑都以它為基準。這讓
//! `pbps` 可以在子目錄中執行 —— 跟 `git` 與 `cargo` 的行為一致，使用者不需要
//! 記得自己站在哪一層。

use std::path::{Path, PathBuf};

pub const CONFIG_FILE: &str = "pbps.yml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("找不到 {CONFIG_FILE}（已從 `{start}` 逐層向上尋找到檔案系統根目錄）")]
    NotFound { start: PathBuf },

    #[error("無法讀取 `{path}`：{source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{path}` 格式錯誤：{message}")]
    Parse { path: PathBuf, message: String },
}

/// 目標資料庫方言。
///
/// 一份專案綁定一個方言 —— 「支援多資料庫」指的是工具能操作不同資料庫，
/// 不是同一份宣告檔能部署到兩者（SPEC §1.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialectName {
    Mssql,
    Postgres,
}

impl DialectName {
    pub const fn as_str(self) -> &'static str {
        match self {
            DialectName::Mssql => "mssql",
            DialectName::Postgres => "postgres",
        }
    }
}

impl std::fmt::Display for DialectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dialect: DialectName,

    #[serde(default = "default_schema_dir")]
    pub schema_dir: PathBuf,

    #[serde(default = "default_ids_file")]
    pub ids_file: PathBuf,
}

fn default_schema_dir() -> PathBuf {
    PathBuf::from("schema")
}

fn default_ids_file() -> PathBuf {
    PathBuf::from("schema.ids.json")
}

impl Config {
    pub fn parse(yaml: &str, path: &Path) -> Result<Self, ConfigError> {
        serde_saphyr::from_str(yaml).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            message: e.to_string(),
        })
    }
}

/// 一個已定位的專案：根目錄加上設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
}

impl Project {
    /// 從 `start` 逐層向上尋找 `pbps.yml`。
    pub fn discover(start: &Path) -> Result<Self, ConfigError> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(CONFIG_FILE);
            if candidate.is_file() {
                return Self::load(&candidate);
            }
            dir = d.parent();
        }
        Err(ConfigError::NotFound {
            start: start.to_owned(),
        })
    }

    /// 直接載入指定的設定檔。
    pub fn load(config_path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(config_path).map_err(|source| ConfigError::Read {
            path: config_path.to_owned(),
            source,
        })?;
        let config = Config::parse(&text, config_path)?;
        // 設定檔一定在某個目錄裡；沒有 parent 只可能是傳入了空路徑。
        let root = config_path.parent().unwrap_or(Path::new(".")).to_owned();
        Ok(Self { root, config })
    }

    /// 宣告檔目錄的絕對（或相對於呼叫端 cwd 的）路徑。
    pub fn schema_dir(&self) -> PathBuf {
        self.root.join(&self.config.schema_dir)
    }

    pub fn ids_file(&self) -> PathBuf {
        self.root.join(&self.config.ids_file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::parse("dialect: mssql\n", Path::new("pbps.yml")).unwrap();
        assert_eq!(c.dialect, DialectName::Mssql);
        assert_eq!(c.schema_dir, PathBuf::from("schema"));
        assert_eq!(c.ids_file, PathBuf::from("schema.ids.json"));
    }

    #[test]
    fn paths_can_be_overridden() {
        let c = Config::parse(
            "dialect: postgres\nschema_dir: db/tables\nids_file: db/ids.json\n",
            Path::new("pbps.yml"),
        )
        .unwrap();
        assert_eq!(c.dialect, DialectName::Postgres);
        assert_eq!(c.schema_dir, PathBuf::from("db/tables"));
    }

    /// dialect 沒有預設值 —— 猜錯方言會產生語法正確但語意錯誤的 SQL。
    #[test]
    fn dialect_is_required() {
        assert!(Config::parse("schema_dir: schema\n", Path::new("pbps.yml")).is_err());
    }

    #[test]
    fn unknown_dialect_is_rejected() {
        assert!(Config::parse("dialect: oracle\n", Path::new("pbps.yml")).is_err());
    }

    /// 拼錯的欄位名要擋下，不能默默使用預設值。
    #[test]
    fn unknown_fields_are_rejected() {
        let err =
            Config::parse("dialect: mssql\nschema_dirs: x\n", Path::new("pbps.yml")).unwrap_err();
        assert!(
            err.to_string().contains("schema_dirs"),
            "錯誤訊息應指出拼錯的欄位：{err}"
        );
    }

    #[test]
    fn discovery_walks_up_from_a_subdirectory() {
        let tmp = std::env::temp_dir().join(format!("pbps-cfg-{}", std::process::id()));
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.join(CONFIG_FILE), "dialect: mssql\n").unwrap();

        let p = Project::discover(&nested).unwrap();
        assert_eq!(p.root, tmp);
        assert_eq!(p.schema_dir(), tmp.join("schema"));
        assert_eq!(p.ids_file(), tmp.join("schema.ids.json"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn discovery_reports_where_it_looked() {
        let tmp = std::env::temp_dir().join(format!("pbps-none-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // 這個目錄沒有 pbps.yml，且 /tmp 之上也不會有
        let err = Project::discover(&tmp).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { .. }));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
