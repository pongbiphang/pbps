//! 載入期的診斷。
//!
//! 分成兩類，刻意用不同的呈現方式：
//!
//! - **YAML 結構錯誤**（語法、未知欄位、型別不符、重複 key）由 `serde-saphyr`
//!   偵測。它的訊息已經包含行號、欄號與帶 caret 的原始碼片段，品質很好，
//!   直接沿用即可 —— 再包一層 miette 只會讓同一段原始碼被印兩次。
//! - **語意錯誤**（型別名無效、表名格式錯誤…）是我們自己的檢查。YAML 本身
//!   合法，錯的是值的內容，因此要自己標出位置，走完整的 miette 診斷。

use std::path::{Path, PathBuf};

use miette::{Diagnostic, NamedSource, SourceSpan};

#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum LoadError {
    #[error("無法讀取 `{path}`")]
    #[diagnostic(code(pbps::load::io))]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{path}` 的 YAML 有問題\n\n{message}")]
    #[diagnostic(code(pbps::load::yaml))]
    Yaml { path: PathBuf, message: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Semantic(#[from] Box<Semantic>),
}

/// 值在語意上無效 —— YAML 讀得懂，但內容不對。
#[derive(Debug, thiserror::Error, Diagnostic)]
#[error("{message}")]
#[diagnostic(code(pbps::load::semantic))]
pub struct Semantic {
    pub message: String,

    #[source_code]
    pub src: NamedSource<String>,

    #[label("{label}")]
    pub span: SourceSpan,

    pub label: String,

    #[help]
    pub help: Option<String>,
}

impl LoadError {
    pub fn semantic(
        src: &SourceFile,
        span: SourceSpan,
        message: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        LoadError::Semantic(Box::new(Semantic {
            message: message.into(),
            src: src.named_source(),
            span,
            label: label.into(),
            help: None,
        }))
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        if let LoadError::Semantic(s) = &mut self {
            s.help = Some(help.into());
        }
        self
    }
}

/// 一份原始檔的內容與名稱，供診斷引用。
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
}

impl SourceFile {
    pub fn new(path: &Path, text: impl Into<String>) -> Self {
        Self {
            name: path.display().to_string(),
            text: text.into(),
        }
    }

    pub fn named_source(&self) -> NamedSource<String> {
        NamedSource::new(&self.name, self.text.clone())
    }
}

/// 把 `serde-saphyr` 的位置轉成 miette 的 span。
///
/// **必須用位元組偏移。** `Span::offset()` 回傳的是字元偏移，miette 期待的是
/// 位元組偏移；只要檔案裡有任何非 ASCII 字元（`description` 幾乎一定有中文），
/// 兩者就會分歧，標記會落在錯誤的位置上。
pub fn to_span(loc: &serde_saphyr::Location) -> SourceSpan {
    let s = loc.span();
    let offset = s.byte_offset().unwrap_or_else(|| s.offset()) as usize;
    let len = s.byte_len().unwrap_or_else(|| s.len()) as usize;
    (offset, len).into()
}
