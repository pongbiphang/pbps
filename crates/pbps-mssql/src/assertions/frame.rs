//! Framing is not a read-only boundary; effective credentials supply that.
use pbps_dialect::Dialect as _;

pub(super) fn single_query(sql: &str) -> bool {
    let lexicon = crate::Mssql.lexicon();
    let code = lexicon.code_only(sql);
    let continues = lexicon.identifier_continues;
    let mut chars = code.chars().peekable();
    let mut depth = 0usize;
    let mut words = Vec::new();
    let mut ended = false;
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '[' || ch == '"' {
            if ended {
                return false;
            }
            let close = if ch == '[' { ']' } else { '"' };
            loop {
                let Some(next) = chars.next() else {
                    return false;
                };
                if next == close && chars.next_if_eq(&close).is_none() {
                    break;
                }
            }
            continue;
        }
        match ch {
            '(' if !ended => depth += 1,
            ')' if !ended && depth > 0 => depth -= 1,
            ';' if depth == 0 => {
                if !words.is_empty() {
                    ended = true;
                }
            }
            _ if ended => return false,
            _ if continues(ch) => {
                let mut word = String::from(ch);
                while let Some(next) = chars.next_if(|c| continues(*c)) {
                    word.push(next);
                }
                // A query expression may start inside parentheses. Keep its
                // first word, then inspect only the outer statement boundary;
                // subqueries and CTE bodies are not additional statements.
                if depth == 0 || words.is_empty() {
                    words.push(word.to_ascii_uppercase());
                }
            }
            _ => {}
        }
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
    fn parenthesized_query_operands_keep_the_outer_statement_boundary() {
        assert!(single_query(
            "(SELECT 1 WHERE 1=0) UNION ALL (SELECT 2 WHERE 1=0)"
        ));
        assert!(single_query("((SELECT 1)) EXCEPT (SELECT 2);"));
        assert!(!single_query("(SELECT 1) UNION ALL (SELECT 2); SELECT 3"));
        assert!(!single_query("(SELECT 1) SELECT 2"));
    }
    #[test]
    fn whole_dialect_identifiers_do_not_expose_ascii_keyword_suffixes() {
        for sql in [
            "SELECT 1 AS 名UPDATE WHERE 1=0",
            "SELECT 1 AS 名COMMIT WHERE 1=0",
            "SELECT 1 AS 名SELECT WHERE 1=0",
        ] {
            assert!(single_query(sql), "{sql}");
        }
        assert!(!single_query("SELECT 1 AS 名UPDATE; COMMIT"));
        assert!(!single_query("SELECT 1 AS 名SELECT SELECT 2"));
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
