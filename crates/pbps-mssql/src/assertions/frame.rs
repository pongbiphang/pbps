//! Framing is not a read-only boundary; effective credentials supply that.
use pbps_dialect::Dialect as _;

pub(super) fn single_query(sql: &str) -> bool {
    let code = crate::Mssql.lexicon().code_only(sql);
    let bytes = code.as_bytes();
    let mut at = 0;
    let mut depth = 0usize;
    let mut words = Vec::new();
    let mut ended = false;
    while at < bytes.len() {
        let ch = bytes[at];
        if ch.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if ch == b'[' || ch == b'"' {
            if ended {
                return false;
            }
            let close = if ch == b'[' { b']' } else { b'"' };
            at += 1;
            loop {
                if at == bytes.len() {
                    return false;
                }
                if bytes[at] == close {
                    at += 1;
                    if bytes.get(at) == Some(&close) {
                        at += 1;
                    } else {
                        break;
                    }
                } else {
                    at += 1;
                }
            }
            continue;
        }
        match ch {
            b'(' if !ended => depth += 1,
            b')' if !ended && depth > 0 => depth -= 1,
            b';' if depth == 0 => {
                if !words.is_empty() {
                    ended = true;
                }
            }
            _ if ended => return false,
            _ if ch.is_ascii_alphabetic() || ch == b'_' || ch == b'@' || ch == b'#' => {
                let start = at;
                at += 1;
                while bytes
                    .get(at)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || b"_@$#".contains(c))
                {
                    at += 1;
                }
                if depth == 0 {
                    words.push(code[start..at].to_ascii_uppercase());
                }
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    if depth != 0
        || !matches!(
            words.first().map(String::as_str),
            Some("SELECT" | "WITH" | "EXEC" | "EXECUTE" | "INSERT" | "UPDATE" | "DELETE")
        )
    {
        return false;
    }
    let mut cases = 0;
    let mut selects = 0;
    let mut set_op = false;
    for (index, word) in words.iter().enumerate() {
        if index == 0
            && matches!(
                word.as_str(),
                "EXEC" | "EXECUTE" | "INSERT" | "UPDATE" | "DELETE"
            )
        {
            continue;
        }
        match word.as_str() {
            "CASE" => cases += 1,
            "END" if cases > 0 => cases -= 1,
            "SELECT" => {
                if selects > 0 && !set_op {
                    return false;
                }
                selects += 1;
                set_op = false;
            }
            "UNION" | "INTERSECT" | "EXCEPT" => set_op = true,
            "ALL" | "DISTINCT" if set_op => {}
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "EXEC" | "EXECUTE" | "CREATE" | "ALTER"
            | "DROP" | "TRUNCATE" | "GRANT" | "DENY" | "REVOKE" | "COMMIT" | "ROLLBACK"
            | "BEGIN" | "END" | "SET" | "WAITFOR" | "DECLARE" | "PRINT" | "RAISERROR" | "THROW"
            | "DBCC" | "USE" | "BACKUP" | "RESTORE" | "KILL" | "RECONFIGURE" | "IF" => {
                return false;
            }
            _ => set_op = false,
        }
    }
    selects > 0 || !matches!(words.first().map(String::as_str), Some("SELECT" | "WITH"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_framing_keeps_literals_comments_ctes_and_set_operations() {
        for sql in [
            "SELECT 1 WHERE 1=0",
            "SELECT CASE WHEN 1=1 THEN 0 END WHERE 1=0",
            "SELECT go FROM t",
            "SELECT '; EXEC' AS [semi;colon] WHERE 1=0; -- COMMIT",
            ";WITH c AS (SELECT 1 n) SELECT n FROM c WHERE 1=0;",
            "SELECT 1 UNION ALL SELECT 2",
            "SELECT (SELECT 1) n",
            "SELECT [SELECT], [a]];b] FROM t",
            "SELECT 1 /* outer /* SELECT 2 */ COMMIT */ WHERE 1=0",
        ] {
            assert!(single_query(sql), "{sql}");
        }
    }
    #[test]
    fn a_second_statement_or_client_batch_never_reaches_the_driver() {
        for sql in [
            "",
            "-- SELECT 1",
            "SELECT 1; SELECT 2",
            "SELECT 1 SELECT 2",
            "COMMIT; INSERT INTO t VALUES(1)",
            "SELECT 1
GO
SELECT 2",
            "SELECT 1; EXEC writer",
            "WITH c AS (SELECT 1 n) DELETE FROM t",
            "SELECT (1",
            "SELECT 1; SET NOCOUNT ON",
        ] {
            assert!(!single_query(sql), "{sql}");
        }
    }
}
