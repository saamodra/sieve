use std::time::{Duration, Instant};

use http::{HeaderMap, HeaderValue};

use super::*;

fn req<'a>(
    method: &'a str,
    host: &'a str,
    path: &'a str,
    headers: &'a HeaderMap,
) -> RequestInfo<'a> {
    RequestInfo {
        method,
        host,
        port: Some(443),
        path,
        headers,
    }
}

fn rule(matcher: Matcher, action: Action) -> Rule {
    Rule {
        matcher,
        action,
        ..Rule::default()
    }
}

#[test]
fn globs_match_wildcards() {
    assert!(glob("*.example.com", "API.example.com"));
    assert!(!glob("*.example.com", "example.com"));
    assert!(glob("a*c*e", "abcde"));
    assert!(!glob("a*c*e", "abde"));
    assert!(glob("exact", "exact"));
    assert!(!glob("exact", "exactly"));
    assert!(glob("*", ""));
    assert!(glob_exact_case("/v1/*", "/v1/users/3"));
    assert!(!glob_exact_case("/V1/*", "/v1/users"));
}

#[test]
fn matchers_combine_with_and() {
    let engine = RuleEngine::new(None);
    engine
        .replace(vec![rule(
            Matcher {
                host: Some("api.*.com".into()),
                path_regex: Some(r"^/users/\d+$".into()),
                method: Some("get".into()),
                header: Some(HeaderMatch {
                    name: "x-debug".into(),
                    value: Some("on*".into()),
                }),
                ..Matcher::default()
            },
            Action {
                block: true,
                ..Action::default()
            },
        )])
        .unwrap();
    let mut headers = HeaderMap::new();
    assert!(
        !engine
            .plan(&req("GET", "api.prod.com", "/users/1", &headers))
            .block
    );
    headers.insert("x-debug", HeaderValue::from_static("ONLINE"));
    assert!(
        engine
            .plan(&req("GET", "api.prod.com", "/users/1", &headers))
            .block
    );
    assert!(
        !engine
            .plan(&req("POST", "api.prod.com", "/users/1", &headers))
            .block
    );
    assert!(
        !engine
            .plan(&req("GET", "api.prod.com", "/users/x", &headers))
            .block
    );
    assert!(
        !engine
            .plan(&req("GET", "other.org", "/users/1", &headers))
            .block
    );
}

#[test]
fn host_patterns_with_a_port_compare_the_port() {
    let engine = RuleEngine::new(None);
    engine
        .replace(vec![rule(
            Matcher {
                host: Some("localhost:3000".into()),
                ..Matcher::default()
            },
            Action {
                breakpoint: true,
                ..Action::default()
            },
        )])
        .unwrap();
    let headers = HeaderMap::new();
    let mut info = req("GET", "localhost", "/", &headers);
    info.port = Some(3000);
    assert!(engine.plan(&info).breakpoint);
    info.port = Some(4000);
    assert!(!engine.plan(&info).breakpoint);
}

#[test]
fn plans_merge_matching_rules_and_skip_disabled_ones() {
    let engine = RuleEngine::new(None);
    let mut disabled = rule(
        Matcher::default(),
        Action {
            block: true,
            ..Action::default()
        },
    );
    disabled.enabled = false;
    let first = rule(
        Matcher::default(),
        Action {
            set_response_headers: [("x-a".to_string(), "1".to_string())].into(),
            delay_ms: Some(10),
            map_remote: Some("localhost:3000".into()),
            ..Action::default()
        },
    );
    let second = rule(
        Matcher::default(),
        Action {
            response_status: Some(201),
            delay_ms: Some(5),
            map_remote: Some("other:1".into()),
            ..Action::default()
        },
    );
    engine.replace(vec![disabled, first, second]).unwrap();
    let headers = HeaderMap::new();
    let plan = engine.plan(&req("GET", "h", "/", &headers));
    assert!(!plan.block);
    assert_eq!(plan.matched.len(), 2);
    assert_eq!(plan.delay, Duration::from_millis(15));
    assert_eq!(plan.response_status, Some(201));
    assert_eq!(plan.map_remote.as_deref(), Some("localhost:3000"));
    assert!(plan.edits_response());
}

#[test]
fn invalid_rules_are_rejected_and_the_old_rules_stay() {
    let engine = RuleEngine::new(None);
    engine.replace(vec![Rule::default()]).unwrap();
    let bad = rule(
        Matcher {
            path_regex: Some("(".into()),
            ..Matcher::default()
        },
        Action::default(),
    );
    assert!(engine.replace(vec![bad]).is_err());
    assert_eq!(engine.rules().len(), 1);
}

#[test]
fn ids_are_assigned_and_kept() {
    let engine = RuleEngine::new(None);
    let named = Rule {
        id: "rule-1".into(),
        ..Rule::default()
    };
    engine.replace(vec![Rule::default(), named]).unwrap();
    let ids: Vec<_> = engine.rules().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, ["rule-2", "rule-1"]);
}

#[test]
fn toml_and_json_files_parse() {
    let toml = r#"
[[rules]]
name = "local backend"
[rules.match]
host = "api.prod.com"
[rules.action]
map_remote = "localhost:3000"
[rules.action.set_request_headers]
x-via = "sieve"

[[rules]]
name = "mock"
[rules.match]
path = "/health"
[rules.action.mock]
status = 503
body = "down"
"#;
    let rules = parse(Path::new("rules.toml"), toml).unwrap();
    assert_eq!(rules.len(), 2);
    assert!(rules[0].enabled);
    assert_eq!(rules[0].action.set_request_headers["x-via"], "sieve");
    assert_eq!(rules[1].action.mock.as_ref().unwrap().status, 503);

    let json =
        r#"{"rules": [{"name": "b", "match": {"method": "DELETE"}, "action": {"block": true}}]}"#;
    let rules = parse(Path::new("rules.json"), json).unwrap();
    assert!(rules[0].action.block);

    for path in ["rules.toml", "rules.json"] {
        let text = render(Path::new(path), &rules).unwrap();
        assert_eq!(parse(Path::new(path), &text).unwrap(), rules);
    }
}

#[test]
fn replace_saves_and_reload_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.toml");
    let engine = RuleEngine::new(Some(path.clone()));
    engine.reload().unwrap();
    assert!(engine.rules().is_empty());
    engine
        .replace(vec![rule(
            Matcher {
                host: Some("a.test".into()),
                ..Matcher::default()
            },
            Action {
                delay_ms: Some(1),
                ..Action::default()
            },
        )])
        .unwrap();

    let other = RuleEngine::new(Some(path));
    other.reload().unwrap();
    assert_eq!(other.rules(), engine.rules());
}

#[test]
fn a_broken_file_keeps_the_previous_rules() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.toml");
    let engine = RuleEngine::new(Some(path.clone()));
    engine.replace(vec![Rule::default()]).unwrap();
    fs::write(&path, "[[rules]\nname =").unwrap();
    assert!(engine.reload().is_err());
    assert_eq!(engine.rules().len(), 1);
}

#[test]
fn the_watcher_reloads_edits_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.toml");
    let engine = RuleEngine::new(Some(path.clone()));
    let _watcher = engine.watch().unwrap();
    fs::write(&path, "[[rules]]\nname = \"one\"\n").unwrap();
    wait_for(|| engine.rules().len() == 1);
    fs::write(
        &path,
        "[[rules]]\nname = \"one\"\n[[rules]]\nname = \"two\"\n",
    )
    .unwrap();
    wait_for(|| engine.rules().len() == 2);
}

fn wait_for(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "the watcher never reloaded the file"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
