//! CF-16 E2E — real server boot (CLI-6 `serve 기동`) over a real socket, then
//! the HTTP-1~5 contract walk from the story's verification method:
//! list → snapshot → events pagination (limit>500 rejected) → WS realtime →
//! budget 3-unit aggregate equality → audit pass-through → error body shapes.
//!
//! Boundary tests from story §8: gateway holds no session state (two fresh
//! instances answer identically), no mutating routes (405), no routes beyond
//! the 4 screens, no outbound client / bathos call in src (static grep), and
//! the screen states (success/empty/error/404) render their designed copy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use gateway::core_port::{AuditStatus, CoreError, CorePort, SessionSummary};
use gateway::stub::StubCore;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn spawn_server(core: Arc<dyn CorePort>) -> (SocketAddr, JoinHandle<()>) {
    let running = gateway::serve::bind(IpAddr::from([127, 0, 0, 1]), 0, core)
        .await
        .expect("bind");
    let addr = running.local_addr;
    let handle = tokio::spawn(async move {
        let _ = running.serve_until_shutdown().await;
    });
    (addr, handle)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn get_json(addr: SocketAddr, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = client()
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    (status, resp.json::<Value>().await.unwrap_or(Value::Null))
}

async fn get_html(addr: SocketAddr, path: &str) -> (reqwest::StatusCode, String) {
    let resp = client()
        .get(format!("http://{addr}{path}"))
        .send()
        .await
        .expect("request");
    let status = resp.status();
    (status, resp.text().await.expect("body"))
}

fn seeded() -> Arc<dyn CorePort> {
    Arc::new(StubCore::seeded())
}

const S1: &str = "01JATS8W6Z9F3A2C5HQ1VMDKR7"; // COMPLETED, sealed+verified
const S3: &str = "01JATSB21E0D04C9A3RGY7VKS5"; // RUNNING, 17 seeded events? (4)
const S4: &str = "01JATSC4D77E22H8N6JQZW3MT9"; // FAILED, sealed+FAILED

// ---------------------------------------------------------------------------
// HTTP-1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http1_list_fields_exactly_the_contract() {
    let (addr, _h) = spawn_server(seeded()).await;
    let (status, body) = get_json(addr, "/api/sessions").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let rows = body.as_array().expect("array");
    assert_eq!(rows.len(), 4);
    for row in rows {
        let keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        // Contract set — no extra columns (팀 필드는 값이 있을 때만 등장).
        for k in &keys {
            assert!(
                [
                    "session_id",
                    "state",
                    "team_id",
                    "budget_spent",
                    "chain_head"
                ]
                .contains(k),
                "unexpected field {k}"
            );
        }
        assert!(keys.contains(&"session_id"));
        assert!(keys.contains(&"state"));
        assert!(keys.contains(&"budget_spent"));
    }
    // The API returns oldest-first (insertion order); D1 reverses for display.
    let first = &rows[0];
    assert_eq!(first["session_id"], "01JATS8W6Z9F3A2C5HQ1VMDKR7"); // S1
    let last = rows.last().unwrap();
    assert_eq!(last["session_id"], S4); // inserted last → D1's top row
    assert_eq!(last["state"], "Failed"); // TYPE-5 wire spelling (PascalCase)
    assert_eq!(last["budget_spent"], 88120);
    assert!(last["chain_head"].is_string());
    // Unsealed session omits chain_head (contract `?`).
    let running = rows.iter().find(|r| r["state"] == "Running").unwrap();
    assert!(running.get("chain_head").is_none());
}

// ---------------------------------------------------------------------------
// HTTP-2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http2_snapshot_and_404_error_shape() {
    let (addr, _h) = spawn_server(seeded()).await;
    let (status, snap) = get_json(addr, &format!("/api/sessions/{S1}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    for key in [
        "session_id",
        "run_id",
        "seed",
        "plan_hash",
        "budget",
        "state",
    ] {
        assert!(snap.get(key).is_some(), "missing SessionHandle field {key}");
    }
    assert_eq!(snap["seed"], 42);
    assert_eq!(snap["budget"]["warn_pct"], 80);

    // 404 + HesmosError body (TYPE-7 field set).
    let (status, err) = get_json(addr, "/api/sessions/does-not-exist").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let e = &err["error"];
    assert_eq!(e["class"], "Compile");
    assert_eq!(e["code"], "SESSION_NOT_FOUND");
    assert!(e["message"].is_string());
    assert!(e["hint"].is_string());
    let keys: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
    for k in keys {
        assert!(
            ["class", "code", "session_id", "node_id", "message", "hint"].contains(&k),
            "unexpected error field {k}"
        );
    }
}

#[tokio::test]
async fn http2_events_pagination_and_limit_guard() {
    let core = StubCore::seeded();
    // 520 external-call events → >500 total for S3 (fixtures add 4 before).
    for i in 0..520 {
        core.push(
            S3,
            "llm.call",
            Some("draft"),
            &[
                ("provider", "glm"),
                ("model", "glm-5.3-flash"),
                ("tokens_in", "10"),
                ("tokens_out", "5"),
                ("latency_ms", "3"),
            ],
            None,
            1_789_612_800_000 + i,
        );
    }
    let (addr, _h) = spawn_server(Arc::new(core)).await;

    // Full first window — no `after` means "from seq 0" (initial-load shape;
    // `?after=0` would be the exclusive continuation past seq 0).
    let (status, page1) = get_json(addr, &format!("/api/sessions/{S3}/events?limit=500")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let p1 = page1.as_array().unwrap();
    assert_eq!(p1.len(), 500);
    assert_eq!(p1[0]["seq"], 0);

    // `?after=0` is exclusive: seq 0 stays out, seq 1 leads.
    let (_, past0) = get_json(
        addr,
        &format!("/api/sessions/{S3}/events?after=0&limit=500"),
    )
    .await;
    assert_eq!(past0.as_array().unwrap()[0]["seq"], 1);

    // Continuation stitches without gap or overlap.
    let (_, page2) = get_json(
        addr,
        &format!("/api/sessions/{S3}/events?after=499&limit=500"),
    )
    .await;
    let p2 = page2.as_array().unwrap();
    assert_eq!(p2[0]["seq"], 500);
    assert_eq!(p1.last().unwrap()["seq"], 499);

    // Default limit = contract max.
    let (_, def) = get_json(addr, &format!("/api/sessions/{S3}/events")).await;
    assert_eq!(def.as_array().unwrap().len(), 500);

    // unbounded 금지 — limit>500 is Usage 400 with the contract body.
    let (status, err) = get_json(addr, &format!("/api/sessions/{S3}/events?limit=501")).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["code"], "USAGE-ARGS");
    assert_eq!(err["error"]["class"], "Usage");

    // Malformed values → same Usage shape.
    let (status, _) = get_json(addr, &format!("/api/sessions/{S3}/events?after=abc")).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    let (status, _) = get_json(addr, &format!("/api/sessions/{S3}/events?limit=0")).await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);

    // Unknown session → 404.
    let (status, _) = get_json(addr, "/api/sessions/nope/events").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // TraceEvent wire fields (TYPE-3 projection + display annotation).
    let (_, one) = get_json(addr, &format!("/api/sessions/{S1}/events?limit=3")).await;
    let ev = &one.as_array().unwrap()[1]; // plan.compiled
    for key in ["seq", "kind", "attrs", "prev_hash", "hash", "ts"] {
        assert!(ev.get(key).is_some(), "missing TraceEvent field {key}");
    }
    assert_eq!(ev["kind"], "plan.compiled");
    assert_eq!(ev["seq"], 1);
}

// ---------------------------------------------------------------------------
// HTTP-3
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http3_ws_streams_only_post_subscription_events() {
    let core = Arc::new(StubCore::seeded());
    let initial_last: u64 = 16; // S1 seeds 17 events (seq 0..=16)
    let (addr, _h) = spawn_server(core.clone()).await;

    let (mut ws, _resp) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/api/sessions/{S1}/stream"))
            .await
            .expect("ws connect");

    // Subscription-point semantics: push probes until one arrives (early
    // probes may legitimately land before the server-side subscribe — they
    // must NOT be replayed later).
    let mut got: Option<Value> = None;
    for i in 0..50u64 {
        core.push(
            S1,
            "llm.call",
            Some("research"),
            &[
                ("provider", "glm"),
                ("model", "glm-5.3-flash"),
                ("tokens_in", "1"),
                ("tokens_out", "1"),
                ("latency_ms", "1"),
            ],
            None,
            1_789_613_000_000 + i,
        );
        match tokio::time::timeout(std::time::Duration::from_millis(150), ws.next()).await {
            Ok(Some(Ok(Message::Text(txt)))) => {
                let ev: Value = serde_json::from_str(&txt).expect("frame json");
                assert_eq!(ev["kind"], "llm.call");
                got = Some(ev);
                break;
            }
            Ok(Some(Ok(other))) => panic!("unexpected frame {other:?}"),
            Ok(Some(Err(e))) => panic!("ws error {e}"),
            Ok(None) => panic!("ws closed"),
            Err(_elapsed) => continue, // probe lost pre-subscribe — keep going
        }
    }
    let ev = got.expect("no realtime frame within the probe window");
    let seq = ev["seq"].as_u64().expect("seq");
    assert!(seq > initial_last, "backfill violation: got seq {seq}");

    // No backfill of the pre-subscription history beyond the one observed.
    match tokio::time::timeout(std::time::Duration::from_millis(300), ws.next()).await {
        Err(_elapsed) => {} // quiet — correct
        Ok(other) => panic!("unexpected extra frame {other:?}"),
    }
    let _ = ws.send(Message::Close(None)).await;
}

// ---------------------------------------------------------------------------
// HTTP-4
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http4_aggregate_is_passthrough_of_the_port() {
    let core = seeded();
    let (addr, _h) = spawn_server(core.clone()).await;

    let (status, body) = get_json(addr, "/api/budgets?team_id=platform").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    for key in ["session", "team", "agent"] {
        assert!(body.get(key).is_some(), "missing 3-unit key {key}");
    }

    // 동일 산출 대조: route output == port report, byte-for-byte (the trap
    // check — the gateway must not recalculate anything).
    let from_port = serde_json::to_value(core.budgets(Some("platform")).unwrap()).unwrap();
    assert_eq!(body, from_port);

    let team = &body["team"];
    let sum: u64 = body["session"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["spent"].as_u64().unwrap())
        .sum();
    assert_eq!(team["spent"], sum);
    assert_eq!(team["spent"], 605_470);
    assert_eq!(team["pct"], 86); // 605,470 / 700,000 → rounded 86%
    assert_eq!(team["warn_count"], 2);
    assert_eq!(team["suspend_count"], 1);
    let warn_row = body["session"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["warn"] == true)
        .unwrap();
    assert_eq!(warn_row["warn_pct"], 80);

    // No filter → all teams.
    let (_, all) = get_json(addr, "/api/budgets").await;
    assert_eq!(all["session"].as_array().unwrap().len(), 4);
}

// ---------------------------------------------------------------------------
// HTTP-5
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http5_audit_passthrough_and_404() {
    let (addr, _h) = spawn_server(seeded()).await;

    let (status, body) = get_json(addr, &format!("/api/audit/{S1}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    // Exactly the 2-field contract set (serde_json maps sort keys, so compare
    // as a set, not by order).
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"chain_head_hash"));
    assert!(keys.contains(&"bathos_audit_verified"));
    assert_eq!(body["bathos_audit_verified"], true);

    // Tampered session verifies FAILED — pass-through, no local recompute.
    let (_, bad) = get_json(addr, &format!("/api/audit/{S4}")).await;
    assert_eq!(bad["bathos_audit_verified"], false);

    // Unsealed → 404 없음.
    let (status, err) = get_json(addr, &format!("/api/audit/{S3}")).await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    assert_eq!(err["error"]["code"], "SESSION_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Read-only boundary (ui-spec §9.0) + route surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mutating_methods_are_rejected_405() {
    let (addr, _h) = spawn_server(seeded()).await;
    let c = client();
    for (method, path) in [
        ("POST", "/api/sessions"),
        ("DELETE", &format!("/api/sessions/{S1}")),
        ("POST", &format!("/api/sessions/{S1}/events")),
        ("POST", "/api/budgets"),
        ("PUT", &format!("/api/audit/{S1}")),
        ("POST", "/"),
        ("POST", "/budgets"),
        ("DELETE", "/audit"),
    ] {
        let req = match method {
            "POST" => c.post(format!("http://{addr}{path}")),
            "PUT" => c.put(format!("http://{addr}{path}")),
            "DELETE" => c.delete(format!("http://{addr}{path}")),
            _ => unreachable!(),
        };
        let status = req.send().await.unwrap().status();
        assert!(
            status == reqwest::StatusCode::METHOD_NOT_ALLOWED
                || status == reqwest::StatusCode::NOT_FOUND,
            "{method} {path} → {status} (조작 엔드포인트 발명 금지)"
        );
        // 200/204가 나오면 상태 변경 경로가 존재한다는 뜻 — 절대 없어야 한다.
        assert_ne!(status, reqwest::StatusCode::OK);
    }
}

#[tokio::test]
async fn routes_beyond_the_four_screens_do_not_exist() {
    let (addr, _h) = spawn_server(seeded()).await;
    for path in [
        "/nope",
        "/sessions",
        "/api",
        "/api/audit",
        "/ws/sessions/x/events",
    ] {
        let (status, _) = get_html(addr, path).await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{path} should 404");
    }
}

// ---------------------------------------------------------------------------
// Screen states (success / empty / error / 404) — 상태 일등 시민
// ---------------------------------------------------------------------------

#[tokio::test]
async fn d1_success_empty_and_error_states() {
    let (addr, _h) = spawn_server(seeded()).await;
    let (status, html) = get_html(addr, "/").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(html.contains("✓ COMPLETED"));
    assert!(html.contains("‖ SUSPENDED"));
    assert!(html.contains(&format!("/sessions/{S1}")));
    assert!(html.contains("aria-current=\"page\""));
    // Numerals are right-aligned mono with tabular-nums (applied via .num in
    // the CSS; the HTML carries the class).
    assert!(html.contains("class=\"num\""));

    let (addr, _h) = spawn_server(Arc::new(StubCore::empty())).await;
    let (_, html) = get_html(addr, "/").await;
    assert!(
        html.contains("아직 세션이 없습니다 — hesmos run &lt;plan&gt;으로 첫 세션을 시작하세요")
    );

    let (addr, _h) = spawn_server(Arc::new(FailingCore)).await;
    let (status, html) = get_html(addr, "/").await;
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(html.contains("세션 목록을 불러오지 못했습니다 — 코어에 연결할 수 없습니다."));
    assert!(html.contains("다시 시도"));
}

#[tokio::test]
async fn d2_render_rules_fold_commit_and_404() {
    let (addr, _h) = spawn_server(seeded()).await;
    let (status, html) = get_html(addr, &format!("/sessions/{S1}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    // P-03 fold: 3 llm.call + 1 tool.call collapsed in the research span.
    assert!(html.contains("(llm.call 3 · tool.call 1 생략 — 12.1s)"));
    // P-10 commit markers + handoff arrow + hash shorthand.
    assert!(html.contains("→ commit #1"));
    assert!(html.contains("research→draft"));
    assert!(html.contains("reason=SCHEMA_VIOLATION"));
    assert!(html.contains("(retry 1/2)"));
    // Seal row annotates the HTTP-5 pass-through (§4.5).
    assert!(html.contains("bathos_audit=verified"));
    // Header P-01 + live badges + aria-live container.
    assert!(html.contains("session ") && html.contains("seed 42"));
    assert!(html.contains("aria-live=\"polite\""));
    assert!(html.contains("재연결 중"));
    assert!(html.contains("LIVE"));

    // Gate filter: gate rows only, no fold line, unfolded gate.fail present.
    let (_, filtered) = get_html(addr, &format!("/sessions/{S1}?filter=gate")).await;
    assert!(filtered.contains("gate.fail"));
    assert!(!filtered.contains("생략"));
    assert!(!filtered.contains("handoff.request"));

    // Unknown session → 404 page copy.
    let (status, html) = get_html(addr, "/sessions/nope").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    assert!(html.contains("세션을 찾을 수 없습니다 — 목록으로 돌아가세요"));
}

#[tokio::test]
async fn d3_team_math_and_d4_seal_list() {
    let (addr, _h) = spawn_server(seeded()).await;

    let (_, d3) = get_html(addr, "/budgets?team_id=platform").await;
    assert!(d3.contains("팀 합계"));
    assert!(d3.contains("605,470 / 700,000 tokens (86%)"));
    assert!(d3.contains("! 80% 경고"));
    assert!(d3.contains("에이전트 상위"));
    assert!(d3.contains("research"));

    let (_, d3_all) = get_html(addr, "/budgets").await;
    assert!(d3_all.contains("전체 팀"));

    let (status, d4) = get_html(addr, "/audit").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(d4.contains("✓ verified"));
    assert!(d4.contains("✗ FAILED"));
    assert!(!d4.contains(S3)); // unsealed sessions are not in the seal list

    // Empty store → both screens show their empty copy.
    let (addr, _h) = spawn_server(Arc::new(StubCore::empty())).await;
    let (_, d3e) = get_html(addr, "/budgets").await;
    assert!(d3e.contains("아직 세션이 없습니다"));
    let (_, d4e) = get_html(addr, "/audit").await;
    assert!(d4e.contains("봉인된 세션이 없습니다 — 세션 종료 시 trace.seal이 기록됩니다"));
}

// ---------------------------------------------------------------------------
// Stateless gateway (SS-24 rule 2) — two fresh instances answer identically
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gateway_adds_no_state_two_instances_identical() {
    let paths = [
        "/api/sessions".to_string(),
        format!("/api/sessions/{S1}/events?limit=500"),
        "/api/budgets?team_id=platform".to_string(),
        format!("/api/audit/{S1}"),
    ];
    let mut first: Vec<(reqwest::StatusCode, Value)> = Vec::new();
    {
        let (addr, _h) = spawn_server(seeded()).await;
        for p in &paths {
            first.push(get_json(addr, p).await);
        }
    } // first server dropped
    {
        let (addr, _h) = spawn_server(seeded()).await;
        for (i, p) in paths.iter().enumerate() {
            let again = get_json(addr, p).await;
            assert_eq!(first[i], again, "instance divergence at {p}");
        }
    }
}

// ---------------------------------------------------------------------------
// serve boot failure → the exit-3 condition (CLI-6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bind_failure_is_reported_not_panicked() {
    let core = seeded();
    let running = gateway::serve::bind(IpAddr::from([127, 0, 0, 1]), 0, core.clone())
        .await
        .expect("first bind");
    let taken = running.local_addr.port();
    // Keep the first server alive in the background while the second bind
    // must fail — that failure is what `hesmos serve` maps to exit 3.
    let keep = tokio::spawn(running.serve_until_shutdown());
    let second = gateway::serve::bind(IpAddr::from([127, 0, 0, 1]), taken, core).await;
    assert!(second.is_err(), "double bind must fail (→ CLI exit 3)");
    keep.abort();
}

// ---------------------------------------------------------------------------
// Static boundary: no outbound client, no bathos call, no gateway state
// ---------------------------------------------------------------------------

#[test]
fn static_boundary_no_outbound_clients_or_engine_calls() {
    use std::fmt::Write as _;
    let src = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
        .expect("src dir")
        .flatten()
        .flat_map(|e| {
            if e.path().is_dir() {
                std::fs::read_dir(e.path())
                    .expect("subdir")
                    .flatten()
                    .map(|e| e.path())
                    .collect::<Vec<_>>()
            } else {
                vec![e.path()]
            }
        })
        .filter(|p| p.extension().map(|x| x == "rs").unwrap_or(false));
    let mut violations = String::new();
    for path in src {
        let text = std::fs::read_to_string(&path).expect("read");
        for (needle, why) in [
            ("reqwest", "outbound HTTP client"),
            ("ureq", "outbound HTTP client"),
            ("hyper_util", "outbound client plumbing"),
            ("hyper::client", "outbound client plumbing"),
            ("TcpStream::connect", "outbound socket"),
            ("bathos::", "direct engine call (P8)"),
            ("hesmos_ffi", "FFI access (D-6)"),
        ] {
            if text.contains(needle) {
                let _ = writeln!(violations, "{}: {}", path.display(), why);
            }
        }
    }
    assert!(violations.is_empty(), "boundary violations:\n{violations}");
}

#[test]
fn static_boundary_routes_hold_no_locks_or_caches() {
    // SS-24 rule 2 — the contact point is stateless. The stub (fake core) may
    // lock; routes/ui/stream may not.
    for rel in [
        "routes/mod.rs",
        "routes/sessions.rs",
        "routes/budgets.rs",
        "routes/audit.rs",
        "stream.rs",
        "ui/mod.rs",
        "serve.rs",
    ] {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/").to_string() + rel)
                .expect("read");
        for needle in ["Mutex", "RwLock", "static mut", "lazy_static", "OnceLock"] {
            assert!(
                !text.contains(needle),
                "{rel} contains {needle} — gateway-owned state is forbidden"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Failing core — drives the error states (P-N7: states are test subjects)
// ---------------------------------------------------------------------------

struct FailingCore;

impl CorePort for FailingCore {
    fn list_sessions(&self) -> Result<Vec<SessionSummary>, CoreError> {
        Err(CoreError::Internal("core query failed".into()))
    }
    fn session(&self, _id: &str) -> Option<gateway::core_port::SessionSnapshot> {
        None
    }
    fn events(
        &self,
        _id: &str,
        _after: Option<u64>,
        _limit: u16,
    ) -> Result<Vec<gateway::core_port::TraceEventWire>, CoreError> {
        Err(CoreError::Internal("core query failed".into()))
    }
    fn budgets(
        &self,
        _team_id: Option<&str>,
    ) -> Result<gateway::core_port::BudgetReport, CoreError> {
        Err(CoreError::Internal("core query failed".into()))
    }
    fn audit(&self, _id: &str) -> Result<Option<AuditStatus>, CoreError> {
        Err(CoreError::Internal("core verify query failed".into()))
    }
    fn subscribe(
        &self,
        _id: &str,
    ) -> Result<tokio::sync::broadcast::Receiver<gateway::core_port::TraceEventWire>, CoreError>
    {
        Err(CoreError::NotFound("x".into()))
    }
}
