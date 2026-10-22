//! Rules: match requests and rewrite, mock, block, redirect or hold them.
//!
//! Rules live in a TOML or JSON file (chosen by extension) that sieve watches,
//! and the UI edits through the same [`RuleEngine`]. A request is checked
//! against every enabled rule in order; the matching rules merge into one
//! [`Plan`] that the proxy applies.
//!
//! ```toml
//! [[rules]]
//! name = "local backend"
//! [rules.match]
//! host = "api.prod.com"
//! [rules.action]
//! map_remote = "localhost:3000"
//! ```

pub mod breakpoint;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use http::HeaderMap;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

pub use breakpoint::{Breakpoints, Decision, HeldRequest, RequestEdit};

/// File name used under the config directory when none is given.
pub const DEFAULT_FILE_NAME: &str = "rules.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Rule {
    /// Stable identifier; assigned when empty.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    #[serde(rename = "match")]
    pub matcher: Matcher,
    pub action: Action,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            matcher: Matcher::default(),
            action: Action::default(),
        }
    }
}

/// Every field that is set must match; an empty matcher matches everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Matcher {
    /// Host glob (`*` matches any run of characters), case-insensitive. A
    /// pattern with a `:port` also compares the port.
    pub host: Option<String>,
    /// Path glob, matched against the path without the query string.
    pub path: Option<String>,
    /// Regular expression searched in the path; combines with `path`.
    pub path_regex: Option<String>,
    pub method: Option<String>,
    pub header: Option<HeaderMatch>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HeaderMatch {
    pub name: String,
    /// Value glob, case-insensitive; without it the header only has to exist.
    pub value: Option<String>,
}

/// What to do with a matching request. Several parts can combine; when more
/// than one of `block`, `mock` and `map_remote` is set, the first of that
/// order wins.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Action {
    /// Answer `403` without contacting the server.
    pub block: bool,
    /// Answer with this response without contacting the server.
    pub mock: Option<Mock>,
    /// Send the request to another server, keeping the path: `localhost:3000`
    /// or `https://staging.example.com`. Without a scheme, `http` is used.
    pub map_remote: Option<String>,
    pub set_request_headers: BTreeMap<String, String>,
    pub remove_request_headers: Vec<String>,
    pub set_response_headers: BTreeMap<String, String>,
    pub remove_response_headers: Vec<String>,
    pub response_status: Option<u16>,
    /// Replaces the response body.
    pub response_body: Option<String>,
    /// Waits this long before answering.
    pub delay_ms: Option<u64>,
    /// Holds the request for editing in the UI before it is sent.
    pub breakpoint: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Mock {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    /// Headers added after `headers`, in order, so a name can repeat (two
    /// `Set-Cookie`, say).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub header_list: Vec<(String, String)>,
    pub body: Option<String>,
    /// Read on every request; relative paths are relative to the rules file.
    pub body_file: Option<PathBuf>,
}

impl Default for Mock {
    fn default() -> Self {
        Self {
            status: 200,
            headers: BTreeMap::new(),
            header_list: Vec::new(),
            body: None,
            body_file: None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct RulesFile {
    rules: Vec<Rule>,
}

/// The request details a rule can match on.
pub struct RequestInfo<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub port: Option<u16>,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
}

/// The merged effect of all rules that matched one request.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub matched: Vec<String>,
    pub block: bool,
    pub mock: Option<Mock>,
    pub map_remote: Option<String>,
    pub set_request_headers: Vec<(String, String)>,
    pub remove_request_headers: Vec<String>,
    pub set_response_headers: Vec<(String, String)>,
    pub remove_response_headers: Vec<String>,
    pub response_status: Option<u16>,
    pub response_body: Option<String>,
    pub delay: Duration,
    pub breakpoint: bool,
    /// Directory that relative mock files resolve against.
    pub base_dir: Option<PathBuf>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.matched.is_empty()
    }

    /// Whether the plan changes the response of a request that is forwarded.
    pub fn edits_response(&self) -> bool {
        !self.set_response_headers.is_empty()
            || !self.remove_response_headers.is_empty()
            || self.response_status.is_some()
            || self.response_body.is_some()
    }
}

struct Compiled {
    rule: Rule,
    path_regex: Option<Regex>,
}

impl Compiled {
    fn new(rule: Rule) -> Result<Self> {
        let path_regex = rule
            .matcher
            .path_regex
            .as_deref()
            .filter(|r| !r.is_empty())
            .map(Regex::new)
            .transpose()
            .with_context(|| format!("rule {:?}: invalid path_regex", rule.name))?;
        if let Some(status) = rule.action.response_status
            && !(100..1000).contains(&status)
        {
            bail!(
                "rule {:?}: response_status {status} is not valid",
                rule.name
            );
        }
        if let Some(mock) = &rule.action.mock
            && !(100..1000).contains(&mock.status)
        {
            bail!(
                "rule {:?}: mock status {} is not valid",
                rule.name,
                mock.status
            );
        }
        Ok(Self { rule, path_regex })
    }

    fn matches(&self, req: &RequestInfo<'_>) -> bool {
        let m = &self.rule.matcher;
        if let Some(host) = non_empty(&m.host) {
            let target = match (host.contains(':'), req.port) {
                (true, Some(port)) => format!("{}:{port}", req.host),
                _ => req.host.to_string(),
            };
            if !glob(host, &target) {
                return false;
            }
        }
        if let Some(path) = non_empty(&m.path)
            && !glob_exact_case(path, req.path)
        {
            return false;
        }
        if let Some(re) = &self.path_regex
            && !re.is_match(req.path)
        {
            return false;
        }
        if let Some(method) = non_empty(&m.method)
            && !req.method.eq_ignore_ascii_case(method.trim())
        {
            return false;
        }
        if let Some(h) = &m.header
            && !h.name.is_empty()
        {
            let mut values = req.headers.get_all(h.name.as_str()).iter().peekable();
            if values.peek().is_none() {
                return false;
            }
            if let Some(pattern) = non_empty(&h.value)
                && !values.any(|v| v.to_str().is_ok_and(|v| glob(pattern, v)))
            {
                return false;
            }
        }
        true
    }
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// `*` matches any run of characters; everything else is literal.
/// Case-insensitive.
fn glob(pattern: &str, text: &str) -> bool {
    glob_with(&pattern.to_lowercase(), &text.to_lowercase())
}

fn glob_exact_case(pattern: &str, text: &str) -> bool {
    glob_with(pattern, text)
}

fn glob_with(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let [first, middle @ .., last] = parts.as_slice() else {
        return pattern == text;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The rule set, its file, and the breakpoints.
pub struct RuleEngine {
    rules: RwLock<Arc<Vec<Compiled>>>,
    path: Option<PathBuf>,
    breakpoints: Breakpoints,
}

impl RuleEngine {
    /// An engine backed by `path`, or kept in memory when `None`. The file is
    /// not read until [`RuleEngine::reload`].
    pub fn new(path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            rules: RwLock::new(Arc::new(Vec::new())),
            path,
            breakpoints: Breakpoints::default(),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn breakpoints(&self) -> &Breakpoints {
        &self.breakpoints
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.snapshot().iter().map(|c| c.rule.clone()).collect()
    }

    fn snapshot(&self) -> Arc<Vec<Compiled>> {
        self.rules.read().expect("rules poisoned").clone()
    }

    /// Reads the rules file. A missing file means no rules; a broken file is an
    /// error and the current rules stay in force.
    pub fn reload(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let rules = parse(path, &text).with_context(|| format!("parsing {}", path.display()))?;
        let count = rules.len();
        self.install(rules)?;
        debug!("loaded {count} rules from {}", path.display());
        Ok(())
    }

    /// Replaces all rules and saves them to the file, if there is one.
    pub fn replace(&self, rules: Vec<Rule>) -> Result<()> {
        let rules = self.install(rules)?;
        if let Some(path) = &self.path {
            let text = render(path, &rules)?;
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    fn install(&self, mut rules: Vec<Rule>) -> Result<Vec<Rule>> {
        assign_ids(&mut rules);
        let compiled = rules
            .iter()
            .cloned()
            .map(Compiled::new)
            .collect::<Result<Vec<_>>>()?;
        *self.rules.write().expect("rules poisoned") = Arc::new(compiled);
        Ok(rules)
    }

    /// Reloads the file whenever it changes, until the returned watcher is
    /// dropped. A file that fails to parse is reported and ignored.
    pub fn watch(self: &Arc<Self>) -> Result<RecommendedWatcher> {
        let path = self
            .path
            .clone()
            .context("this rule engine has no file to watch")?;
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let file_name = path.file_name().map(|n| n.to_os_string());
        let engine = self.clone();
        // Watch the directory: editors save by replacing the file, which a
        // watch on the file itself would lose.
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else { return };
                if event.kind.is_access() {
                    return;
                }
                if !event
                    .paths
                    .iter()
                    .any(|p| p.file_name() == file_name.as_deref())
                {
                    return;
                }
                match engine.reload() {
                    Ok(()) => info!("rules reloaded"),
                    Err(e) => warn!("keeping the previous rules: {e:#}"),
                }
            })?;
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }

    /// Merges every enabled rule that matches `req`.
    pub fn plan(&self, req: &RequestInfo<'_>) -> Plan {
        let mut plan = Plan {
            base_dir: self
                .path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            ..Plan::default()
        };
        for compiled in self.snapshot().iter() {
            let rule = &compiled.rule;
            if !rule.enabled || !compiled.matches(req) {
                continue;
            }
            let a = &rule.action;
            plan.matched.push(if rule.name.is_empty() {
                rule.id.clone()
            } else {
                rule.name.clone()
            });
            plan.block |= a.block;
            plan.breakpoint |= a.breakpoint;
            if plan.mock.is_none() {
                plan.mock.clone_from(&a.mock);
            }
            if plan.map_remote.is_none() {
                plan.map_remote = a.map_remote.clone().filter(|r| !r.trim().is_empty());
            }
            plan.set_request_headers
                .extend(a.set_request_headers.clone());
            plan.remove_request_headers
                .extend(a.remove_request_headers.clone());
            plan.set_response_headers
                .extend(a.set_response_headers.clone());
            plan.remove_response_headers
                .extend(a.remove_response_headers.clone());
            plan.response_status = a.response_status.or(plan.response_status);
            if a.response_body.is_some() {
                plan.response_body.clone_from(&a.response_body);
            }
            plan.delay += Duration::from_millis(a.delay_ms.unwrap_or(0));
        }
        plan
    }
}

fn assign_ids(rules: &mut [Rule]) {
    let mut next = 1;
    let mut taken: Vec<String> = rules.iter().map(|r| r.id.clone()).collect();
    for rule in rules.iter_mut() {
        if !rule.id.is_empty() {
            continue;
        }
        let id = loop {
            let candidate = format!("rule-{next}");
            next += 1;
            if !taken.contains(&candidate) {
                break candidate;
            }
        };
        taken.push(id.clone());
        rule.id = id;
    }
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

fn parse(path: &Path, text: &str) -> Result<Vec<Rule>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file: RulesFile = if is_json(path) {
        serde_json::from_str(text)?
    } else {
        toml::from_str(text)?
    };
    Ok(file.rules)
}

fn render(path: &Path, rules: &[Rule]) -> Result<String> {
    let file = RulesFile {
        rules: rules.to_vec(),
    };
    Ok(if is_json(path) {
        serde_json::to_string_pretty(&file)?
    } else {
        toml::to_string_pretty(&file)?
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod header_list_tests {
    use super::*;

    const OLD: &str = r#"
[[rules]]
name = "mock"
[rules.match]
host = "a.test"
[rules.action.mock]
status = 201
headers = { x-one = "1" }
body = "hi"
"#;

    #[test]
    fn a_mock_without_header_list_parses_and_round_trips() {
        let rules = parse(Path::new("rules.toml"), OLD).unwrap();
        let mock = rules[0].action.mock.as_ref().unwrap();
        assert!(mock.header_list.is_empty());

        let json = serde_json::to_value(&rules[0]).unwrap();
        assert!(json["action"]["mock"].get("header_list").is_none());
        let back: Rule = serde_json::from_value(json).unwrap();
        assert_eq!(back, rules[0]);
    }

    #[test]
    fn header_list_round_trips_through_json_and_toml() {
        let mut rules = parse(Path::new("rules.toml"), OLD).unwrap();
        rules[0].action.mock.as_mut().unwrap().header_list = vec![
            ("set-cookie".into(), "a=1".into()),
            ("set-cookie".into(), "b=2".into()),
        ];
        for name in ["rules.json", "rules.toml"] {
            let path = Path::new(name);
            let text = render(path, &rules).unwrap();
            assert_eq!(parse(path, &text).unwrap(), rules, "{name}");
        }
    }
}
