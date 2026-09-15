//! Bind the accepted Unix socket to dockerd, including systemd activation.
//! SO_PEERCRED names the creator of a listener inherited through fd://, not
//! its acceptor. Linux UNIX_DIAG_PEER supplies the actual connected socket;
//! a protected PID file is only a candidate, never proof of that ownership.

use super::{ProcessLease, UnqualifiedProcess, proc_base, process_gone, read_bounded};
use rustix::net::{self, netlink, sockopt};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;

pub(crate) struct DaemonLease {
    process: ProcessLease,
    socket: UnixPeer,
}

impl DaemonLease {
    pub(crate) async fn capture(stream: &UnixStream) -> Result<Self, UnqualifiedProcess> {
        let peer = stream.peer_cred().map_err(|_| UnqualifiedProcess)?;
        if peer.uid() != 0 {
            return Err(UnqualifiedProcess);
        }
        let creator = ProcessLease::capture(
            peer.pid()
                .ok_or(UnqualifiedProcess)?
                .try_into()
                .map_err(|_| UnqualifiedProcess)?,
        )?;
        let process = match creator
            .executable_path()
            .file_name()
            .and_then(|name| name.to_str())
        {
            Some("dockerd") => creator,
            Some("systemd") if creator.pid() == 1 => {
                let path = Path::new("/run/docker.pid");
                for parent in path.ancestors().skip(1) {
                    let metadata = std::fs::metadata(parent).map_err(|_| UnqualifiedProcess)?;
                    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                        return Err(UnqualifiedProcess);
                    }
                }
                let file = std::fs::File::open(path).map_err(|_| UnqualifiedProcess)?;
                let metadata = file.metadata().map_err(|_| UnqualifiedProcess)?;
                if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                    return Err(UnqualifiedProcess);
                }
                use std::io::Read as _;
                let mut pid = String::new();
                file.take(33)
                    .read_to_string(&mut pid)
                    .map_err(|_| UnqualifiedProcess)?;
                if pid.len() > 32 {
                    return Err(UnqualifiedProcess);
                }
                let process =
                    ProcessLease::capture(pid.trim().parse().map_err(|_| UnqualifiedProcess)?)?;
                creator.check()?;
                process
            }
            _ => return Err(UnqualifiedProcess),
        };
        if process
            .executable_path()
            .file_name()
            .is_none_or(|name| name != "dockerd")
        {
            return Err(UnqualifiedProcess);
        }
        let status = read_bounded(&proc_base(&process.directory).join("status"), 16384)?;
        if !status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .is_some_and(|uids| uids.split_whitespace().collect::<Vec<_>>() == ["0"; 4])
        {
            return Err(UnqualifiedProcess);
        }
        // connect() can finish before accept() installs the server descriptor.
        // Wait only for ownership of this exact source-free connection; never
        // reconnect and never send a Docker request to an unqualified peer.
        let socket = UnixPeer::capture(stream, &process).await?;
        let lease = Self { process, socket };
        lease.check()?;
        Ok(lease)
    }

    pub(crate) fn check(&self) -> Result<(), UnqualifiedProcess> {
        self.socket.check(&self.process)
    }

    pub(crate) fn same_process(&self, other: &Self) -> Result<bool, UnqualifiedProcess> {
        self.check()?;
        other.check()?;
        self.process.same_process(&other.process)
    }
}

struct UnixPeer {
    inode: u32,
    cookie: u64,
    peer: u32,
}

impl UnixPeer {
    async fn capture(
        stream: &UnixStream,
        process: &ProcessLease,
    ) -> Result<Self, UnqualifiedProcess> {
        let inode = std::fs::metadata(format!("/proc/self/fd/{}", stream.as_raw_fd()))
            .map_err(|_| UnqualifiedProcess)?
            .ino()
            .try_into()
            .map_err(|_| UnqualifiedProcess)?;
        let cookie = sockopt::socket_cookie(stream).map_err(|_| UnqualifiedProcess)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                process.check()?;
                let peer = unix_peer(inode, cookie)?;
                if peer != 0 && owns_socket(process, peer)? {
                    return Ok(Self {
                        inode,
                        cookie,
                        peer,
                    });
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| UnqualifiedProcess)?
    }

    fn check(&self, process: &ProcessLease) -> Result<(), UnqualifiedProcess> {
        process.check()?;
        if unix_peer(self.inode, self.cookie)? != self.peer || !owns_socket(process, self.peer)? {
            return Err(UnqualifiedProcess);
        }
        process.check()
    }
}

fn owns_socket(process: &ProcessLease, inode: u32) -> Result<bool, UnqualifiedProcess> {
    let expected = PathBuf::from(format!("socket:[{inode}]"));
    let entries = std::fs::read_dir(proc_base(&process.directory).join("fd"))
        .map_err(|_| UnqualifiedProcess)?;
    let mut owned = false;
    for (count, entry) in entries.enumerate() {
        if count >= 65536 {
            return Err(UnqualifiedProcess);
        }
        match std::fs::read_link(entry.map_err(|_| UnqualifiedProcess)?.path()) {
            Ok(path) => owned |= path == expected,
            Err(error) if process_gone(&error) => (),
            Err(_) => return Err(UnqualifiedProcess),
        }
    }
    Ok(owned)
}

fn unix_peer(inode: u32, cookie: u64) -> Result<u32, UnqualifiedProcess> {
    let socket = net::socket_with(
        net::AddressFamily::NETLINK,
        net::SocketType::DGRAM,
        net::SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )
    .map_err(|_| UnqualifiedProcess)?;
    let kernel = netlink::SocketAddrNetlink::new(0, 0);
    net::connect(&socket, &kernel).map_err(|_| UnqualifiedProcess)?;
    for timeout in [sockopt::Timeout::Recv, sockopt::Timeout::Send] {
        sockopt::set_socket_timeout(&socket, timeout, Some(Duration::from_millis(100)))
            .map_err(|_| UnqualifiedProcess)?;
    }
    // One exact SOCK_DIAG_BY_FAMILY request. No namespace-wide socket dump.
    let sequence: u32 = rand::random();
    let mut request = Vec::with_capacity(40);
    request.extend(40_u32.to_ne_bytes());
    request.extend(20_u16.to_ne_bytes());
    request.extend(1_u16.to_ne_bytes()); // NLM_F_REQUEST
    request.extend(sequence.to_ne_bytes());
    request.extend(0_u32.to_ne_bytes());
    request.extend([1, 0, 0, 0]); // AF_UNIX, protocol, padding
    request.extend(u32::MAX.to_ne_bytes());
    request.extend(inode.to_ne_bytes());
    request.extend(4_u32.to_ne_bytes()); // UDIAG_SHOW_PEER
    request.extend((cookie as u32).to_ne_bytes());
    request.extend(((cookie >> 32) as u32).to_ne_bytes());
    if net::send(&socket, &request, net::SendFlags::empty()).map_err(|_| UnqualifiedProcess)?
        != request.len()
    {
        return Err(UnqualifiedProcess);
    }
    let mut buffer = [0_u8; 512];
    let (length, actual, sender) = net::recvfrom(&socket, &mut buffer[..], net::RecvFlags::TRUNC)
        .map_err(|_| UnqualifiedProcess)?;
    if length != actual
        || sender.and_then(|s| netlink::SocketAddrNetlink::try_from(s).ok()) != Some(kernel)
    {
        return Err(UnqualifiedProcess);
    }
    parse_peer(&buffer[..length], sequence, inode, cookie)
}

fn parse_peer(
    reply: &[u8],
    sequence: u32,
    inode: u32,
    cookie: u64,
) -> Result<u32, UnqualifiedProcess> {
    let word = |offset| {
        reply
            .get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_ne_bytes)
            .ok_or(UnqualifiedProcess)
    };
    if reply.len() < 32 || word(0)? as usize != reply.len()
        || u16::from_ne_bytes([reply[4], reply[5]]) != 20 || reply[6..8] != [0, 0] || word(8)? != sequence
        || reply[16..20] != [1, 1, 1, 0] // AF_UNIX, SOCK_STREAM, ESTABLISHED
        || word(20)? != inode || word(24)? != cookie as u32 || word(28)? != (cookie >> 32) as u32
    {
        return Err(UnqualifiedProcess);
    }
    let mut peer = None;
    let mut offset = 32;
    while offset < reply.len() {
        let header = reply.get(offset..offset + 4).ok_or(UnqualifiedProcess)?;
        let length = u16::from_ne_bytes([header[0], header[1]]) as usize;
        let kind = u16::from_ne_bytes([header[2], header[3]]);
        if length < 4 || offset + length > reply.len() {
            return Err(UnqualifiedProcess);
        }
        if kind == 2 {
            // UNIX_DIAG_PEER
            if length != 8 || peer.is_some() {
                return Err(UnqualifiedProcess);
            }
            peer = Some(word(offset + 4)?);
        }
        offset += (length + 3) & !3;
    }
    if offset != reply.len() {
        return Err(UnqualifiedProcess);
    }
    peer.ok_or(UnqualifiedProcess)
}

#[cfg(test)]
mod tests;
