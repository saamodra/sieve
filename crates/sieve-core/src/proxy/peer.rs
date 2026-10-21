//! Finds the local process on the other end of a loopback connection, so the
//! proxy can intercept some programs and leave the rest alone (macOS only).

use std::net::SocketAddr;

/// Executable path of the process that owns the client end of a loopback
/// connection from `client` to the proxy. `None` when it can't be told, e.g.
/// the process belongs to another user or already exited.
pub(crate) async fn client_process_path(client: SocketAddr) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // netstat would be faster, but launched from another program it lists
        // no TCP sockets on recent macOS. libproc needs `unsafe` (a C union)
        // and libclang at build time.
        let out = tokio::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP@{}", lsof_addr(client)), "-Fpn"])
            .output()
            .await
            .ok()?;
        let pid = find_pid(&String::from_utf8_lossy(&out.stdout), client)?;
        // On macOS `comm` is the full executable path; `-ww` stops truncation.
        let out = tokio::process::Command::new("ps")
            .args(["-ww", "-o", "comm=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then_some(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = client;
        None
    }
}

/// How lsof writes an endpoint: `127.0.0.1:52805`, `[::1]:52805`.
fn lsof_addr(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(a) => format!("{}:{}", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("[{}]:{}", a.ip(), a.port()),
    }
}

/// Finds the pid in `lsof -Fpn` output whose socket has `client` as its local
/// end, i.e. the client's socket rather than the proxy's end of it.
fn find_pid(lsof: &str, client: SocketAddr) -> Option<i32> {
    let local = format!("{}->", lsof_addr(client));
    let mut pid = None;
    for line in lsof.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if line
            .strip_prefix('n')
            .is_some_and(|n| n.starts_with(&local))
        {
            return pid;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The proxy (pid 900) and the client (pid 61230) both hold an end of the
    // connection, so lsof reports both.
    const LSOF: &str = "\
p900
f12
n127.0.0.1:8080->127.0.0.1:52805
p61230
f31
n127.0.0.1:52805->127.0.0.1:8080
";

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn finds_the_client_end_not_the_proxy_end() {
        assert_eq!(find_pid(LSOF, addr("127.0.0.1:52805")), Some(61230));
    }

    #[test]
    fn handles_ipv6_loopback() {
        let out = "p77\nf4\nn[::1]:52807->[::1]:8080\n";
        assert_eq!(find_pid(out, addr("[::1]:52807")), Some(77));
        assert_eq!(lsof_addr(addr("[::1]:52807")), "[::1]:52807");
    }

    #[test]
    fn unknown_connection_has_no_pid() {
        assert_eq!(find_pid(LSOF, addr("127.0.0.1:1")), None);
        assert_eq!(find_pid("", addr("127.0.0.1:1")), None);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn finds_this_test_process() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_, client) = listener.accept().await.unwrap();
        let path = client_process_path(client).await.unwrap();
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(std::path::Path::new(&path), exe);
    }
}
