//! One way to read a set of provider-owned objects so that the answer can be relied on
//! (DEC-2447.12 to .14): the same machinery for a type, an operator class and an operator, which
//! had been written three times.
//!
//! The whole set is read twice and compared, one object against itself. In the caller's
//! READ COMMITTED transaction a savepoint gives the separate catalog queries of one read no
//! common snapshot (DECISIONS 423), and even in a snapshot of our own the deparsing functions see
//! the latest catalog, so an object that changed between the queries would otherwise come back as
//! a definition that never existed, with a pin to match. Both passes cover every object, so a
//! change to the first while a later one is read is seen by the second. Optimistic, like 423's
//! revalidation: a change after the last read is the replay's to see. An object that differs
//! between the passes is unreadable, never absent. A failed read is every object unreadable:
//! nothing was learned, and nothing is reported as absent.
//!
//! The read runs under an empty `search_path` of its own, set for the transaction and not left
//! behind (`format_type` and the deparsed definitions qualify a name only when the path does not
//! show it). Inside the caller's transaction it runs under a savepoint that is rolled back
//! afterwards, so the path it sets goes with it (DECISIONS 418).

use pbps_db::{Conn, DbError};
use pbps_dialect::TransactionFraming;

use crate::pins::Within;

/// `REPEATABLE READ`, so the lookup, the definition and the closure of one object come from one
/// snapshot of the catalog tables.
pub(crate) const OWN_TRANSACTION: TransactionFraming = TransactionFraming {
    begin: std::borrow::Cow::Borrowed("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;"),
    commit: std::borrow::Cow::Borrowed("COMMIT;"),
    rollback: std::borrow::Cow::Borrowed("ROLLBACK;"),
};

/// A kind of provider-owned object and how one is read. What differs per kind stays with the kind;
/// the double read and the transaction framing are here.
pub(crate) trait Kind {
    type Wanted;
    type Read: PartialEq;
    /// The savepoint a read inside the caller's transaction takes.
    const SAVEPOINT: &'static str;
    /// The word a message uses for the object ("type", "operator class", "operator").
    const NOUN: &'static str;

    /// The answer for an object whose read failed or changed, with the reason.
    fn unreadable(why: String) -> Self::Read;

    async fn read_one(conn: &mut Conn, wanted: &Self::Wanted) -> Result<Self::Read, DbError>;
}

async fn read_each<K: Kind>(
    conn: &mut Conn,
    wanted: &[K::Wanted],
) -> Result<Vec<K::Read>, DbError> {
    conn.query("SELECT pg_catalog.set_config('search_path', '', true) AS was_set")
        .await?;
    let mut passes = [Vec::new(), Vec::new()];
    for pass in &mut passes {
        for one in wanted {
            pass.push(K::read_one(conn, one).await?);
        }
    }
    let [first, second] = passes;
    Ok(first
        .into_iter()
        .zip(second)
        .map(|(first, second)| settled::<K>(first, second))
        .collect())
}

/// The read both passes agree on, or an unreadable one when they do not.
pub(crate) fn settled<K: Kind>(first: K::Read, second: K::Read) -> K::Read {
    if first == second {
        first
    } else {
        K::unreadable(format!("the {} changed while it was being read", K::NOUN))
    }
}

async fn read_all<K: Kind>(
    conn: &mut Conn,
    within: Within,
    wanted: &[K::Wanted],
) -> Result<Vec<K::Read>, DbError> {
    match within {
        Within::OwnTransaction => {
            conn.begin(&OWN_TRANSACTION).await?;
            let result = read_each::<K>(conn, wanted).await;
            let closed = conn.rollback(&OWN_TRANSACTION).await;
            let reads = result?;
            closed?;
            Ok(reads)
        }
        Within::CallersTransaction => {
            let savepoint = K::SAVEPOINT;
            conn.execute(&format!("SAVEPOINT {savepoint}")).await?;
            let result = read_each::<K>(conn, wanted).await;
            // Leave the caller's transaction usable if it can be: a failed statement aborts it
            // until the savepoint is rolled back. The read's own error is the one reported.
            let undone = conn
                .execute(&format!("ROLLBACK TO SAVEPOINT {savepoint}"))
                .await;
            let released = conn
                .execute(&format!("RELEASE SAVEPOINT {savepoint}"))
                .await;
            let reads = result?;
            undone?;
            released?;
            Ok(reads)
        }
    }
}

/// One answer per object, in order.
pub(crate) async fn read<K: Kind>(
    conn: &mut Conn,
    within: Within,
    wanted: &[K::Wanted],
) -> Vec<K::Read> {
    match read_all::<K>(conn, within, wanted).await {
        Ok(reads) => reads,
        Err(e) => wanted
            .iter()
            .map(|_| K::unreadable(e.to_string()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake kind: the shared rules hold for any object, and the message names the kind's noun
    /// (so a class's refusal still says "class" and an operator's "operator").
    struct Fake;

    impl Kind for Fake {
        type Wanted = u8;
        type Read = Result<u8, String>;
        const SAVEPOINT: &'static str = "fake";
        const NOUN: &'static str = "widget";

        fn unreadable(why: String) -> Self::Read {
            Err(why)
        }

        async fn read_one(_: &mut Conn, wanted: &u8) -> Result<Self::Read, DbError> {
            Ok(Ok(*wanted))
        }
    }

    #[test]
    fn two_passes_that_agree_are_the_read_and_two_that_differ_are_unreadable_by_the_kinds_noun() {
        assert_eq!(settled::<Fake>(Ok(1), Ok(1)), Ok(1));
        assert_eq!(
            settled::<Fake>(Err("x".into()), Err("x".into())),
            Err("x".into())
        );
        for (first, second) in [
            (Ok(1), Ok(2)),
            (Ok(1), Err("gone".into())),
            (Err("a".into()), Ok(1)),
        ] {
            let read = settled::<Fake>(first, second);
            assert_eq!(
                read,
                Err("the widget changed while it was being read".into())
            );
        }
        // The kinds in the crate say their own word.
        assert_eq!(
            crate::class_read::ClassRead::Unreadable(
                "the class changed while it was being read".into()
            ),
            settled::<crate::class_read::Classes>(
                crate::class_read::ClassRead::Absent,
                crate::class_read::ClassRead::Unreadable(String::new())
            )
        );
        assert_eq!(
            crate::operator_read::OperatorRead::Unreadable(
                "the operator changed while it was being read".into()
            ),
            settled::<crate::operator_read::Operators>(
                crate::operator_read::OperatorRead::Absent,
                crate::operator_read::OperatorRead::Unreadable(String::new())
            )
        );
    }
}
