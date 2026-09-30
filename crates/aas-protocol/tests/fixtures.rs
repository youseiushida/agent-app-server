//! Golden fixtures shared with the Android client.
//!
//! `golden_fixtures_are_up_to_date` fails when `fixtures/protocol/` differs from what
//! `aas_protocol::examples::fixtures()` produces. Regenerate with:
//!
//! ```text
//! AAS_UPDATE_FIXTURES=1 cargo test -p aas-protocol --test fixtures
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use aas_protocol::events::EventEnvelope;
use aas_protocol::examples::fixtures;
use aas_protocol::methods::{ClientRequest, result_roundtrip};
use aas_protocol::notifications::ServerNotification;
use aas_protocol::rpc::{ErrorKind, MessageKind, RpcMessage};
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/protocol")
}

fn render(value: &Value) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("fixture serializes");
    s.push('\n');
    s
}

fn existing_files(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel);
            }
        }
    }
    out
}

#[test]
fn golden_fixtures_are_up_to_date() {
    let dir = fixtures_dir();
    let update = std::env::var_os("AAS_UPDATE_FIXTURES").is_some();
    let expected = fixtures();
    let mut problems = Vec::new();
    let mut expected_paths = BTreeSet::new();

    for fixture in &expected {
        assert!(
            expected_paths.insert(fixture.path.clone()),
            "duplicate fixture path {}",
            fixture.path
        );
        let path = dir.join(&fixture.path);
        let content = render(&fixture.value);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        } else {
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing.replace("\r\n", "\n") == content => {}
                Ok(_) => problems.push(format!("changed: {}", fixture.path)),
                Err(_) => problems.push(format!("missing: {}", fixture.path)),
            }
        }
    }

    for stale in existing_files(&dir).difference(&expected_paths) {
        if update {
            std::fs::remove_file(dir.join(stale)).unwrap();
        } else {
            problems.push(format!("stale: {stale}"));
        }
    }

    assert!(
        problems.is_empty(),
        "fixtures are out of date:\n  {}\nregenerate with: AAS_UPDATE_FIXTURES=1 cargo test -p aas-protocol --test fixtures",
        problems.join("\n  ")
    );
}

#[test]
fn every_fixture_round_trips_through_the_types() {
    for fixture in fixtures() {
        let (dir, name) = fixture.path.split_once('/').unwrap();
        let name = name.trim_end_matches(".json");
        match dir {
            "requests" => {
                let msg: RpcMessage = serde_json::from_value(fixture.value.clone()).unwrap();
                assert_eq!(msg.kind(), MessageKind::Request, "{}", fixture.path);
                let method = msg.method.clone().unwrap();
                // `<method>.json`, or `<method>_<variant>.json` for another shape of its params.
                let method_file = method.replace('/', "_");
                assert!(
                    name == method_file
                        || name
                            .strip_prefix(method_file.as_str())
                            .is_some_and(|variant| variant.len() > 1 && variant.starts_with('_')),
                    "{}: named after another method than {method}",
                    fixture.path
                );
                let parsed = ClientRequest::parse(&method, msg.params.clone())
                    .unwrap_or_else(|e| panic!("{}: {e}", fixture.path));
                assert_eq!(
                    parsed.params_json(),
                    msg.params.unwrap(),
                    "{}",
                    fixture.path
                );
            }
            "responses" => {
                let msg: RpcMessage = serde_json::from_value(fixture.value.clone()).unwrap();
                assert_eq!(msg.kind(), MessageKind::Response, "{}", fixture.path);
                let method = name.replace('_', "/");
                let result = msg.result.unwrap();
                let back = result_roundtrip(&method, result.clone()).unwrap();
                assert_eq!(back, result, "{}", fixture.path);
            }
            "events" => {
                let env: EventEnvelope = serde_json::from_value(fixture.value.clone())
                    .unwrap_or_else(|e| panic!("{}: {e}", fixture.path));
                assert_eq!(
                    serde_json::to_value(&env).unwrap(),
                    fixture.value,
                    "{}",
                    fixture.path
                );
            }
            "notifications" => {
                let msg: RpcMessage = serde_json::from_value(fixture.value.clone()).unwrap();
                assert_eq!(msg.kind(), MessageKind::Notification, "{}", fixture.path);
                let note = ServerNotification::parse(
                    msg.method.as_deref().unwrap(),
                    msg.params.clone().unwrap(),
                )
                .unwrap_or_else(|e| panic!("{}: {e}", fixture.path));
                assert_eq!(note.params_json(), msg.params.unwrap(), "{}", fixture.path);
            }
            "errors" => {
                let msg: RpcMessage = serde_json::from_value(fixture.value.clone()).unwrap();
                let err = msg.error.unwrap();
                let kind = err.kind().unwrap();
                assert_eq!(kind.as_str(), name, "{}", fixture.path);
                assert_eq!(ErrorKind::from_code(err.code), Some(kind));
            }
            "http" => {}
            other => panic!("unexpected fixture directory {other}"),
        }
    }
}
