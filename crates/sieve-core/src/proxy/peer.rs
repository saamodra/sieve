//! Finds the local process on the other end of a loopback connection, so the
//! proxy can intercept some programs and leave the rest alone (macOS only).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A pid keeps its executable path this long. Pids are handed out
/// sequentially, so one is not reused for a different program within seconds;
/// the TTL only bounds the damage of a process that execs something else.
const PATH_TTL: Duration = Duration::from_secs(30);
const PATH_CACHE_CAP: usize = 256;

static PATHS: LazyLock<Mutex<PathCache>> =
    LazyLock::new(|| Mutex::new(PathCache::new(PATH_TTL, PATH_CACHE_CAP)));

/// The last `lsof` listing of loopback TCP sockets, and when that `lsof`
/// started. Holding the lock while `lsof` runs keeps it to one process at a
/// time, so a burst of connections queues instead of forking one each.
#[cfg(target_os = "macos")]
static LSOF: tokio::sync::Mutex<Option<(Instant, std::sync::Arc<str>)>> =
    tokio::sync::Mutex::const_new(None);

/// Executable path of the process that owns the client end of a loopback
/// connection from `client` to the proxy. `None` when it can't be told, e.g.
/// the process belongs to another user or already exited.
pub(crate) async fn client_process_path(client: SocketAddr) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let sockets = loopback_sockets(Instant::now()).await?;
        let pid = find_pid(&sockets, client)?;
        if let Some(path) = PATHS.lock().ok()?.get(pid, Instant::now()) {
            return Some(path);
        }
        // On macOS `comm` is the full executable path; `-ww` stops truncation.
        let out = tokio::process::Command::new("ps")
            .args(["-ww", "-o", "comm=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            return None;
        }
        PATHS.lock().ok()?.insert(pid, path.clone(), Instant::now());
        Some(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = client;
        None
    }
}

/// Loopback TCP sockets of every process, as `lsof -Fpn` output. A listing
/// that started after `asked` already contains the caller's connection, so
/// callers that queued behind one `lsof` share the next one.
#[cfg(target_os = "macos")]
async fn loopback_sockets(asked: Instant) -> Option<std::sync::Arc<str>> {
    let mut last = LSOF.lock().await;
    if let Some((started, sockets)) = last.as_ref()
        && *started >= asked
    {
        return Some(sockets.clone());
    }
    let started = Instant::now();
    // netstat would be faster, but launched from another program it lists
    // no TCP sockets on recent macOS. libproc needs `unsafe` (a C union)
    // and libclang at build time. `-b` and `-w` skip lsof's blocking
    // kernel calls and warnings.
    let out = tokio::process::Command::new("lsof")
        .args(["-nPbw", "-i4TCP@127.0.0.1", "-i6TCP@[::1]", "-Fpn"])
        .output()
        .await
        .ok()?;
    let sockets: std::sync::Arc<str> = String::from_utf8_lossy(&out.stdout).into();
    *last = Some((started, sockets.clone()));
    Some(sockets)
}

/// Executable paths by pid, so `ps` runs once per process rather than once per
/// connection. Callers pass the current time, which keeps expiry testable.
struct PathCache {
    ttl: Duration,
    cap: usize,
    entries: HashMap<i32, (String, Instant)>,
}

impl PathCache {
    fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, pid: i32, now: Instant) -> Option<String> {
        let (path, stored) = self.entries.get(&pid)?;
        if now.saturating_duration_since(*stored) < self.ttl {
            return Some(path.clone());
        }
        self.entries.remove(&pid);
        None
    }

    fn insert(&mut self, pid: i32, path: String, now: Instant) {
        if self.entries.len() >= self.cap && !self.entries.contains_key(&pid) {
            let ttl = self.ttl;
            self.entries
                .retain(|_, (_, stored)| now.saturating_duration_since(*stored) < ttl);
            if self.entries.len() >= self.cap {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, stored))| *stored)
                    .map(|(pid, _)| *pid);
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(pid, (path, now));
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn finds_this_test_process_over_ipv6() {
        let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
            return;
        };
        let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (_, client) = listener.accept().await.unwrap();
        let path = client_process_path(client).await.unwrap();
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(std::path::Path::new(&path), exe);
    }

    #[test]
    fn cache_hit_within_ttl() {
        let t0 = Instant::now();
        let mut cache = PathCache::new(Duration::from_secs(30), 8);
        cache.insert(10, "/bin/a".into(), t0);
        assert_eq!(
            cache.get(10, t0 + Duration::from_secs(29)).as_deref(),
            Some("/bin/a")
        );
        assert_eq!(cache.get(11, t0), None);
    }

    #[test]
    fn cache_entry_expires_after_ttl() {
        let t0 = Instant::now();
        let mut cache = PathCache::new(Duration::from_secs(30), 8);
        cache.insert(10, "/bin/a".into(), t0);
        assert_eq!(cache.get(10, t0 + Duration::from_secs(30)), None);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn cache_reinsert_refreshes_the_entry() {
        let t0 = Instant::now();
        let mut cache = PathCache::new(Duration::from_secs(30), 8);
        cache.insert(10, "/bin/a".into(), t0);
        cache.insert(10, "/bin/b".into(), t0 + Duration::from_secs(20));
        assert_eq!(
            cache.get(10, t0 + Duration::from_secs(40)).as_deref(),
            Some("/bin/b")
        );
    }

    #[test]
    fn cache_is_bounded_and_drops_expired_before_live() {
        let t0 = Instant::now();
        let mut cache = PathCache::new(Duration::from_secs(30), 3);
        cache.insert(1, "/bin/1".into(), t0);
        cache.insert(2, "/bin/2".into(), t0 + Duration::from_secs(20));
        cache.insert(3, "/bin/3".into(), t0 + Duration::from_secs(21));
        // Full: the expired entry (1) goes, the live ones stay.
        cache.insert(4, "/bin/4".into(), t0 + Duration::from_secs(40));
        assert_eq!(cache.entries.len(), 3);
        assert_eq!(cache.get(1, t0 + Duration::from_secs(40)), None);
        assert!(cache.get(2, t0 + Duration::from_secs(40)).is_some());
    }

    #[test]
    fn cache_evicts_the_oldest_when_all_entries_are_live() {
        let t0 = Instant::now();
        let mut cache = PathCache::new(Duration::from_secs(30), 3);
        for pid in 1..=5 {
            cache.insert(
                pid,
                format!("/bin/{pid}"),
                t0 + Duration::from_secs(pid as u64),
            );
        }
        assert_eq!(cache.entries.len(), 3);
        let now = t0 + Duration::from_secs(6);
        assert_eq!(cache.get(1, now), None);
        assert_eq!(cache.get(2, now), None);
        assert!(cache.get(5, now).is_some());
    }

    /// The lookup as it was before the cache and the shared listing: one `lsof`
    /// and one `ps` per call, all in parallel when called concurrently.
    #[cfg(target_os = "macos")]
    async fn uncached_process_path(client: SocketAddr) -> Option<String> {
        let out = tokio::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP@{}", lsof_addr(client)), "-Fpn"])
            .output()
            .await
            .ok()?;
        let pid = find_pid(&String::from_utf8_lossy(&out.stdout), client)?;
        let out = tokio::process::Command::new("ps")
            .args(["-ww", "-o", "comm=", "-p", &pid.to_string()])
            .output()
            .await
            .ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!path.is_empty()).then_some(path)
    }

    /// Opens `n` loopback connections to a listener in this process and
    /// returns the accepted server-side addresses, whose peer is the client
    /// end. The streams are returned so the sockets stay open.
    #[cfg(target_os = "macos")]
    async fn loopback_clients(n: usize) -> (Vec<SocketAddr>, Vec<tokio::net::TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = listener.local_addr().unwrap();
        let mut addrs = Vec::new();
        let mut streams = Vec::new();
        for _ in 0..n {
            let client = tokio::net::TcpStream::connect(server).await.unwrap();
            let (accepted, peer) = listener.accept().await.unwrap();
            addrs.push(peer);
            streams.push(client);
            streams.push(accepted);
        }
        (addrs, streams)
    }

    #[cfg(target_os = "macos")]
    fn report(label: &str, mut times: Vec<Duration>, wall: Duration) {
        times.sort();
        let median = times[times.len() / 2];
        let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
        println!(
            "{label:<28} n={:<3} median={median:>9.1?} p95={p95:>9.1?} wall={wall:>9.1?}",
            times.len()
        );
    }

    /// `cargo test -p sieve-core peer -- --ignored --nocapture`
    ///
    /// Times the process lookup per connection, sequentially and with every
    /// connection looked up at once, before and after the change.
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "measures lsof/ps on the host; run by hand"]
    async fn measure_lookup_cost() {
        const N: usize = 50;
        let (addrs, _streams) = loopback_clients(N).await;

        macro_rules! time_one {
            ($lookup:expr, $addr:expr) => {{
                let start = Instant::now();
                let path = $lookup($addr).await;
                assert!(path.is_some(), "lookup failed for {}", $addr);
                start.elapsed()
            }};
        }

        let start = Instant::now();
        let mut times = Vec::new();
        for &addr in &addrs {
            times.push(time_one!(uncached_process_path, addr));
        }
        report("before, sequential", times, start.elapsed());

        let start = Instant::now();
        let tasks: Vec<_> = addrs
            .iter()
            .map(|&addr| tokio::spawn(async move { time_one!(uncached_process_path, addr) }))
            .collect();
        let mut times = Vec::new();
        for task in tasks {
            times.push(task.await.unwrap());
        }
        report("before, concurrent", times, start.elapsed());

        // Both runs below start from a cold cache.
        PATHS.lock().unwrap().entries.clear();
        *super::LSOF.lock().await = None;
        let start = Instant::now();
        let mut times = Vec::new();
        for &addr in &addrs {
            times.push(time_one!(client_process_path, addr));
        }
        report("after, sequential", times, start.elapsed());

        PATHS.lock().unwrap().entries.clear();
        *super::LSOF.lock().await = None;
        let start = Instant::now();
        let tasks: Vec<_> = addrs
            .iter()
            .map(|&addr| tokio::spawn(async move { time_one!(client_process_path, addr) }))
            .collect();
        let mut times = Vec::new();
        for task in tasks {
            times.push(task.await.unwrap());
        }
        report("after, concurrent", times, start.elapsed());
    }
}
