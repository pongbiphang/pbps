//! Whether the supplied server is this run's alone.
//!
//! The engine has no reachable network: its namespace holds one loopback
//! device and every session to it is a TCP connection whose both ends are in
//! that namespace's own table. So the kernel's answer is complete for what
//! reaches the engine over its port — every established row is either one
//! end of a session this run opened, or an intruder — and the engine's own
//! session list and cumulative counter are the independent second signal.

use super::{Error, Signal};
use crate::resolver::native::{ProcessLease, UnqualifiedProcess, socket_owners};
use std::collections::BTreeSet;

/// One session's two ends in the engine's network namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TcpPair {
    client: u64,
    server: u64,
    local: String,
    peer: String,
}

impl TcpPair {
    fn inodes(&self) -> [u64; 2] {
        [self.client, self.server]
    }
}

/// Binds a session just opened through a forwarder to the actual kernel
/// endpoints: the one established pair to the engine's port whose client end
/// the forwarder's processes hold. The server end's holder is the engine's
/// backend, returned so the engine's own report of it can be compared.
pub(crate) fn bind(
    init: &ProcessLease,
    forwarder: &ProcessLease,
    port: u16,
    known: &[&TcpPair],
) -> Result<(TcpPair, ProcessLease), &'static str> {
    let rows = established(init).map_err(|_| "the engine's TCP table is unreadable")?;
    let server_address = format!("0100007F:{port:04X}");
    let mut found = None;
    for row in &rows {
        if row.peer != server_address || known.iter().any(|pair| pair.client == row.inode) {
            continue;
        }
        let owners = socket_owners(forwarder, row.inode)
            .map_err(|_| "the forwarder's descriptors are unreadable")?;
        if owners.is_empty() {
            continue;
        }
        let server = rows
            .iter()
            .find(|candidate| candidate.local == server_address && candidate.peer == row.local)
            .ok_or("the session's server end is not in the engine's table")?;
        if found
            .replace(TcpPair {
                client: row.inode,
                server: server.inode,
                local: row.local.clone(),
                peer: row.peer.clone(),
            })
            .is_some()
        {
            return Err("the forwarder holds more than one session to the engine");
        }
    }
    let pair = found.ok_or("no session held by the forwarder reaches the engine's port")?;
    let mut backends =
        socket_owners(init, pair.server).map_err(|_| "the engine's descriptors are unreadable")?;
    if backends.len() != 1 {
        return Err("the session's server end is not held by exactly one engine process");
    }
    Ok((pair, backends.remove(0)))
}

/// The pair is still there, still held by the same backend.
pub(crate) fn still_bound(
    init: &ProcessLease,
    pair: &TcpPair,
    backend: &ProcessLease,
) -> Result<(), UnqualifiedProcess> {
    backend.check()?;
    let rows = established(init)?;
    let present = |inode, local: &str, peer: &str| {
        rows.iter()
            .any(|row| row.inode == inode && row.local == local && row.peer == peer)
    };
    if !present(pair.client, &pair.local, &pair.peer)
        || !present(pair.server, &pair.peer, &pair.local)
    {
        return Err(UnqualifiedProcess);
    }
    let owners = socket_owners(init, pair.server)?;
    if owners.len() != 1 || !owners[0].same_process(backend)? {
        return Err(UnqualifiedProcess);
    }
    backend.check()
}

/// Every socket in the engine's network namespace is accounted for.
///
/// A listener is the engine's and reachable from nowhere else. A row with no
/// inode is a connection already gone, which the counter answers for. Any
/// other row is a session: one of ours, or a refusal. A Unix socket of any
/// kind is a refusal — the recipe gives the engine no Unix listener, so one
/// is something a process inside the container made.
pub(crate) fn census(init: &ProcessLease, pairs: &[&TcpPair]) -> Result<(), Error> {
    let unreadable = |_| Error::Exclusivity(Signal::Unreadable);
    let ours: BTreeSet<u64> = pairs.iter().flat_map(|pair| pair.inodes()).collect();
    let mut seen = BTreeSet::new();
    for table in ["net/tcp", "net/tcp6"] {
        let text = init.read_proc(table, 8 * 1024 * 1024).map_err(unreadable)?;
        for row in tcp_rows(&text).map_err(unreadable)? {
            let row = row.map_err(unreadable)?;
            if row.state == "0A" || row.inode == 0 {
                continue;
            }
            if !ours.contains(&row.inode) {
                return Err(Error::Exclusivity(Signal::ForeignSocket));
            }
            seen.insert(row.inode);
        }
    }
    if seen != ours {
        return Err(Error::Exclusivity(Signal::MissingChannel));
    }
    let unix = init
        .read_proc("net/unix", 4 * 1024 * 1024)
        .map_err(unreadable)?;
    if unix_rows(&unix).map_err(unreadable)? != 0 {
        return Err(Error::Exclusivity(Signal::ForeignSocket));
    }
    init.check().map_err(unreadable)
}

/// Compares the engine's own view of its client sessions with this run's.
///
/// The kernel census decides admission; this is the independent second
/// signal. A server that cannot report every session errors in the engine
/// adapter rather than reporting an idle server.
pub(crate) fn only_our_sessions(reported: &[String], ours: &BTreeSet<String>) -> Result<(), Error> {
    let reported: BTreeSet<_> = reported.iter().cloned().collect();
    if reported != *ours {
        return Err(Error::Exclusivity(Signal::SessionList));
    }
    Ok(())
}

struct TcpRow {
    local: String,
    peer: String,
    state: String,
    inode: u64,
}

fn established(init: &ProcessLease) -> Result<Vec<TcpRow>, UnqualifiedProcess> {
    let text = init.read_proc("net/tcp", 8 * 1024 * 1024)?;
    tcp_rows(&text)?
        .filter(|row| row.as_ref().is_ok_and(|row| row.state == "01") || row.is_err())
        .collect()
}

fn tcp_rows(
    table: &str,
) -> Result<impl Iterator<Item = Result<TcpRow, UnqualifiedProcess>>, UnqualifiedProcess> {
    let mut lines = table.lines();
    if !lines
        .next()
        .is_some_and(|header| header.contains("local_address"))
    {
        return Err(UnqualifiedProcess);
    }
    Ok(lines.map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 10 {
            return Err(UnqualifiedProcess);
        }
        Ok(TcpRow {
            local: fields[1].to_owned(),
            peer: fields[2].to_owned(),
            state: fields[3].to_owned(),
            inode: fields[9].parse().map_err(|_| UnqualifiedProcess)?,
        })
    }))
}

/// How many Unix sockets a `net/unix` table lists. A table this parser
/// cannot read is not an empty one.
fn unix_rows(table: &str) -> Result<usize, UnqualifiedProcess> {
    let mut lines = table.lines();
    if !lines.next().is_some_and(|header| header.starts_with("Num")) {
        return Err(UnqualifiedProcess);
    }
    let mut count = 0;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 7 || fields.len() > 8 {
            return Err(UnqualifiedProcess);
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    fn row(index: u32, local: &str, peer: &str, state: &str, inode: u64) -> String {
        format!(
            "{index:4}: {local} {peer} {state} 00000000:00000000 00:00000000 00000000   999        0 {inode} 1 0000000000000000 100 0 0 10 0\n"
        )
    }

    #[test]
    fn listeners_and_dead_rows_are_not_sessions_but_every_other_row_is() {
        let table = format!(
            "{TCP_HEADER}{}{}{}{}",
            row(0, "00000000:1538", "00000000:0000", "0A", 100),
            row(1, "0100007F:A001", "0100007F:1538", "01", 201),
            row(2, "0100007F:1538", "0100007F:A001", "01", 202),
            row(3, "0100007F:A002", "0100007F:1538", "06", 0),
        );
        let rows: Vec<_> = tcp_rows(&table).unwrap().map(Result::unwrap).collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].inode, 201);
        assert!(tcp_rows("no header\n").is_err());
        assert!(
            tcp_rows(&format!("{TCP_HEADER}short row\n"))
                .unwrap()
                .next()
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn a_truncated_or_unreadable_unix_table_is_not_an_empty_one() {
        assert_eq!(
            unix_rows("Num       RefCount Protocol Flags    Type St Inode Path\n").unwrap(),
            0
        );
        assert_eq!(
            unix_rows("Num       RefCount Protocol Flags    Type St Inode Path\n0000000000000000: 00000002 00000000 00010000 0001 01 28833013 /var/run/postgresql/.s.PGSQL.5432\n").unwrap(),
            1
        );
        assert!(unix_rows("").is_err());
        assert!(unix_rows("Num\ngarbage\n").is_err());
    }

    #[test]
    fn an_engine_session_this_run_did_not_open_is_never_tolerated() {
        let ours = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        only_our_sessions(&["b".to_owned(), "a".to_owned()], &ours).unwrap();
        assert!(matches!(
            only_our_sessions(&["a".to_owned()], &ours),
            Err(Error::Exclusivity(Signal::SessionList))
        ));
        assert!(matches!(
            only_our_sessions(&["a".to_owned(), "b".to_owned(), "c".to_owned()], &ours),
            Err(Error::Exclusivity(Signal::SessionList))
        ));
    }
}
