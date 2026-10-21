//! The self-contained `/setup/ios` page.

/// Renders the page. `host` is the `host[:port]` the phone used, or empty.
pub fn render(host: &str) -> String {
    let pac_url = if host.is_empty() {
        "/proxy.pac".to_string()
    } else {
        format!("http://{host}/proxy.pac")
    };
    TEMPLATE.replace("{{PAC_URL}}", &pac_url)
}

const TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sieve iOS setup</title>
<style>
  :root { color-scheme: light dark; --bg:#f6f7f9; --fg:#14171a; --card:#fff; --muted:#5b6670; --accent:#0a66ff; --ok:#1a9f4b; --line:#dde2e7; }
  @media (prefers-color-scheme: dark) { :root { --bg:#101214; --fg:#eef0f2; --card:#1a1d20; --muted:#9aa5b0; --accent:#5a9bff; --ok:#3ccf74; --line:#2b3035; } }
  * { box-sizing: border-box; }
  body { margin:0; padding:16px; background:var(--bg); color:var(--fg); font:16px/1.5 -apple-system, system-ui, sans-serif; }
  main { max-width:560px; margin:0 auto; }
  h1 { font-size:1.4rem; margin:8px 0 4px; }
  p.lead { color:var(--muted); margin:0 0 16px; }
  .card { background:var(--card); border:1px solid var(--line); border-radius:12px; padding:16px; margin-bottom:12px; }
  .card h2 { font-size:1rem; margin:0 0 8px; }
  ol { margin:0; padding-left:1.2em; }
  li { margin-bottom:6px; }
  .btn { display:block; text-align:center; background:var(--accent); color:#fff; text-decoration:none; padding:14px; border-radius:10px; font-weight:600; min-height:44px; }
  code { background:var(--bg); border:1px solid var(--line); border-radius:6px; padding:2px 6px; word-break:break-all; }
  #status { display:flex; align-items:center; gap:12px; }
  #mark { width:32px; height:32px; border-radius:50%; border:2px solid var(--line); display:grid; place-items:center; color:transparent; font-weight:700; flex:none; }
  #status.ok #mark { background:var(--ok); border-color:var(--ok); color:#fff; }
</style>
</head>
<body>
<main>
  <h1>Set up this iPhone for sieve</h1>
  <p class="lead">Three steps. Safari must be used for the profile download.</p>

  <div class="card">
    <h2>1. Install the certificate profile</h2>
    <a class="btn" href="/setup/ios.mobileconfig">Download profile</a>
    <ol style="margin-top:12px">
      <li>Tap <b>Allow</b> when Safari asks to download a configuration profile.</li>
      <li>Open <b>Settings</b> and tap <b>Profile Downloaded</b>, then <b>Install</b>. It shows as "Not Verified", which is expected.</li>
    </ol>
  </div>

  <div class="card">
    <h2>2. Trust the certificate</h2>
    <ol>
      <li>Go to <b>Settings &gt; General &gt; About &gt; Certificate Trust Settings</b>.</li>
      <li>Turn on full trust for <b>sieve CA</b>.</li>
    </ol>
  </div>

  <div class="card">
    <h2>3. Set the Wi-Fi proxy</h2>
    <ol>
      <li>Go to <b>Settings &gt; Wi-Fi</b> and tap the <b>i</b> next to your network.</li>
      <li>Scroll to <b>Configure Proxy</b> and choose <b>Automatic</b>.</li>
      <li>Set URL to <code id="pac">{{PAC_URL}}</code> and tap <b>Save</b>.</li>
    </ol>
  </div>

  <div class="card">
    <div id="status"><span id="mark">&#10003;</span><span id="msg">Waiting for HTTPS traffic from this device&hellip;</span></div>
  </div>
</main>
<script>
  (function () {
    var pac = document.getElementById("pac");
    if (pac.textContent.charAt(0) === "/") pac.textContent = location.origin + pac.textContent;
    var box = document.getElementById("status");
    var msg = document.getElementById("msg");
    function poll() {
      fetch("/setup/ios/status", { cache: "no-store" })
        .then(function (r) { return r.json(); })
        .then(function (s) {
          if (s.ok) {
            box.className = "ok";
            msg.textContent = "All set. HTTPS traffic is coming through.";
          } else {
            setTimeout(poll, 2000);
          }
        })
        .catch(function () { setTimeout(poll, 4000); });
    }
    poll();
  })();
</script>
</body>
</html>
"##;
