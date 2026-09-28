//! Who is behind the admin listener (design.md §12).
//!
//! The admin token protects the admin API from the other accounts of the PC. The CLI must
//! therefore never send it to a listener of another account: any local account can bind
//! `server.admin_listen` while the daemon does not hold it (before the user logs on, during the
//! watchdog's restart delay, after `stop`). Before a request carries the token, the connection
//! that was just established is checked: the process owning its server end (from the system's
//! TCP connection table, `GetExtendedTcpTable`) must run as the same Windows user as the CLI
//! (the user SID of the process tokens). The check is about exactly the socket the token is
//! then written to, so the listener cannot change in between.

use tokio::net::TcpStream;

/// Fails unless the server end of `stream` belongs to a process of the current user.
pub fn ensure_same_user(stream: &TcpStream) -> anyhow::Result<()> {
    imp::ensure_same_user(stream)
}

#[cfg(windows)]
mod imp {
    use std::net::{IpAddr, SocketAddr};

    use anyhow::{Context, bail};
    use tokio::net::TcpStream;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows::Win32::Security::{
        GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub fn ensure_same_user(stream: &TcpStream) -> anyhow::Result<()> {
        let (ours, theirs) = (stream.local_addr()?, stream.peer_addr()?);
        // The server end of this connection: its local address is our peer address.
        let pid = connection_owner(theirs, ours)?
            .with_context(|| format!("the process listening on {theirs} cannot be identified; the admin token is not sent"))?;
        if pid == std::process::id() {
            // The daemon runs in this very process (tests of the daemon in-process).
            return Ok(());
        }
        let owner = process_user(pid).with_context(|| {
            format!("the process listening on {theirs} (pid {pid}) is not one of this user's; the admin token is not sent")
        })?;
        let current = current_user()?;
        if owner != current {
            bail!(
                "the process listening on {theirs} (pid {pid}) runs as another user; the admin token is not sent"
            );
        }
        Ok(())
    }

    /// The owning process of the TCP connection whose local end is `local` and whose remote
    /// end is `remote`, from the system's connection table.
    pub(super) fn connection_owner(
        local: SocketAddr,
        remote: SocketAddr,
    ) -> anyhow::Result<Option<u32>> {
        match (local.ip(), remote.ip()) {
            (IpAddr::V4(l), IpAddr::V4(r)) => {
                let table = tcp_table(AF_INET.0 as u32)?;
                Ok(rows::<MIB_TCPROW_OWNER_PID>(&table)
                    .iter()
                    .find(|row| {
                        row.dwLocalAddr == u32::from_ne_bytes(l.octets())
                            && port(row.dwLocalPort) == local.port()
                            && row.dwRemoteAddr == u32::from_ne_bytes(r.octets())
                            && port(row.dwRemotePort) == remote.port()
                    })
                    .map(|row| row.dwOwningPid))
            }
            (IpAddr::V6(l), IpAddr::V6(r)) => {
                let table = tcp_table(AF_INET6.0 as u32)?;
                Ok(rows::<MIB_TCP6ROW_OWNER_PID>(&table)
                    .iter()
                    .find(|row| {
                        row.ucLocalAddr == l.octets()
                            && port(row.dwLocalPort) == local.port()
                            && row.ucRemoteAddr == r.octets()
                            && port(row.dwRemotePort) == remote.port()
                    })
                    .map(|row| row.dwOwningPid))
            }
            _ => bail!(
                "the two ends of the connection {local} / {remote} are of different address families"
            ),
        }
    }

    /// Ports in the table are in network byte order in the low 16 bits.
    fn port(raw: u32) -> u16 {
        u16::from_be(raw as u16)
    }

    /// The connection table (`MIB_TCPTABLE_OWNER_PID` / `MIB_TCP6TABLE_OWNER_PID`) of one
    /// address family, as 8-byte aligned words. The table may grow between asking for its size
    /// and reading it; the read is repeated with the size the system reports then.
    fn tcp_table(family: u32) -> anyhow::Result<Vec<u64>> {
        let mut size = 0u32;
        let mut buf: Vec<u64> = Vec::new();
        loop {
            let ptr = if buf.is_empty() {
                None
            } else {
                Some(buf.as_mut_ptr().cast())
            };
            // SAFETY: `buf` holds at least `size` writable bytes whenever a pointer is passed.
            let status = unsafe {
                GetExtendedTcpTable(
                    ptr,
                    &mut size,
                    false,
                    family,
                    TCP_TABLE_OWNER_PID_CONNECTIONS,
                    0,
                )
            };
            match status {
                s if s == NO_ERROR.0 && !buf.is_empty() => return Ok(buf),
                s if s == NO_ERROR.0 || s == ERROR_INSUFFICIENT_BUFFER.0 => {
                    buf = vec![0u64; (size as usize).div_ceil(std::mem::size_of::<u64>()).max(1)];
                }
                s => bail!("reading the TCP connection table failed (error {s})"),
            }
        }
    }

    /// The rows of a table read by [`tcp_table`]: a `u32` count, then the rows (aligned).
    fn rows<Row: Copy>(table: &[u64]) -> Vec<Row> {
        let base = table.as_ptr().cast::<u8>();
        // SAFETY: the table starts with its entry count.
        let count = unsafe { base.cast::<u32>().read_unaligned() } as usize;
        let offset = std::mem::align_of::<Row>().max(std::mem::size_of::<u32>());
        let available =
            std::mem::size_of_val(table).saturating_sub(offset) / std::mem::size_of::<Row>();
        (0..count.min(available))
            // SAFETY: row `i` lies within the buffer (bounded by `available`).
            .map(|i| unsafe {
                base.add(offset + i * std::mem::size_of::<Row>())
                    .cast::<Row>()
                    .read_unaligned()
            })
            .collect()
    }

    /// Closes a handle when dropped.
    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle is owned by this value and closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// The user SID (its binary form) of process `pid`.
    pub(super) fn process_user(pid: u32) -> anyhow::Result<Vec<u8>> {
        // SAFETY: plain call; the handle is owned below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .with_context(|| format!("cannot open process {pid}"))?;
        let process = Owned(process);
        token_user(process.0)
    }

    pub(super) fn current_user() -> anyhow::Result<Vec<u8>> {
        // SAFETY: the pseudo handle of the current process needs no closing.
        token_user(unsafe { GetCurrentProcess() })
    }

    fn token_user(process: HANDLE) -> anyhow::Result<Vec<u8>> {
        let mut token = HANDLE::default();
        // SAFETY: `token` is a live out-parameter; the handle is owned below.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
            .context("cannot read the process token")?;
        let token = Owned(token);
        let mut len = 0u32;
        // SAFETY: a size query (no buffer); it fails with the needed length.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(std::mem::size_of::<u64>()).max(1)];
        // SAFETY: `buf` holds `len` writable, aligned bytes.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            )
        }
        .context("cannot read the user of the process token")?;
        // SAFETY: the buffer now starts with a TOKEN_USER whose SID points into the buffer.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` is a valid SID inside `buf`, which outlives this read.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        // SAFETY: as above; the SID is `sid_len` bytes long.
        Ok(unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), sid_len) }.to_vec())
    }
}

#[cfg(not(windows))]
mod imp {
    use tokio::net::TcpStream;

    pub fn ensure_same_user(stream: &TcpStream) -> anyhow::Result<()> {
        let peer = stream.peer_addr()?;
        anyhow::bail!(
            "the owner of the admin listener {peer} can only be verified on Windows; the admin token is not sent"
        )
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_listener_of_this_user_is_accepted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_accepted, _) = listener.accept().await.unwrap();
        let (ours, theirs) = (stream.local_addr().unwrap(), stream.peer_addr().unwrap());
        assert_eq!(
            imp::connection_owner(theirs, ours).unwrap(),
            Some(std::process::id()),
            "the server end is ours"
        );
        assert_eq!(
            imp::connection_owner(ours, theirs).unwrap(),
            Some(std::process::id()),
            "and so is the client end"
        );
        ensure_same_user(&stream).unwrap();
        assert_eq!(
            imp::process_user(std::process::id()).unwrap(),
            imp::current_user().unwrap()
        );
    }

    #[tokio::test]
    async fn ipv6_connections_are_found_too() {
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return;
        };
        let stream = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_accepted, _) = listener.accept().await.unwrap();
        let (ours, theirs) = (stream.local_addr().unwrap(), stream.peer_addr().unwrap());
        assert_eq!(
            imp::connection_owner(theirs, ours).unwrap(),
            Some(std::process::id())
        );
    }

    #[test]
    fn a_process_of_another_account_is_refused() {
        // PID 4 is always the System process, which runs as LocalSystem: either its token
        // cannot be read from this account, or its user differs.
        let current = imp::current_user().unwrap();
        if let Ok(user) = imp::process_user(4) {
            assert_ne!(user, current);
        }
    }
}
