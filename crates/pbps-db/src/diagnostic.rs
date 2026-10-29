//! Transient driver facts; never a saved report (DEC-2465.2).
#[derive(Clone, PartialEq, Eq)]
pub enum ErrorPosition {
    Original(u32),
    Internal { position: u32, query: String },
}
impl std::fmt::Debug for ErrorPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Original(p) => f.debug_tuple("Original").field(p).finish(),
            Self::Internal { position, .. } => f
                .debug_struct("Internal")
                .field("position", position)
                .field("query", &"[redacted]")
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverDiagnostic {
    pub primary: String,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub datatype: Option<String>,
    pub constraint: Option<String>,
    pub position: Option<ErrorPosition>,
}
