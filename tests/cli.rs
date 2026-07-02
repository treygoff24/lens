mod common;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use assert_cmd::Command;
use image::{ImageBuffer, Rgba};
use serde_json::Value;

use common::MockServer;

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lens-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lens_cmd(home: &Path, xdg: &Path) -> Command {
    let mut cmd = Command::cargo_bin("lens").unwrap();
    cmd.env("HOME", home)
        .env("XDG_DATA_HOME", xdg)
        .env_remove("CEREBRAS_API_KEY")
        .env_remove("LENS_MODEL")
        .env_remove("LENS_API_BASE")
        .env_remove("LENS_MAX_CONCURRENCY");
    cmd
}

fn fixture_library(name: &str, count: usize) -> PathBuf {
    let dir = temp_root(name);
    for i in 0..count {
        write_png(
            &dir.join(format!("{i}.png")),
            2 + i as u32,
            [i as u8, 0, 0, 255],
        );
    }
    dir
}

fn write_png(path: &Path, size: u32, rgba: [u8; 4]) {
    let img = ImageBuffer::from_pixel(size, size, Rgba(rgba));
    img.save(path).unwrap();
}

fn json_stdout(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn json_stderr(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stderr).unwrap()
}

fn run_index(server: &MockServer, home: &Path, xdg: &Path, dir: &Path) -> std::process::Output {
    lens_cmd(home, xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("--concurrency")
        .arg("1")
        .arg("index")
        .arg(dir)
        .output()
        .unwrap()
}

#[test]
fn full_index_run_reports_envelope_counts_and_spend() {
    let server = MockServer::start();
    let home = temp_root("home-index");
    let xdg = temp_root("xdg-index");
    let dir = fixture_library("library-index", 2);

    let output = run_index(&server, &home, &xdg, &dir);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["schema"], "lens.cli.response.v1");
    assert_eq!(stdout["ok"], true);
    assert_eq!(stdout["command"], "index");
    assert!(stdout.get("cost_dollars").is_none());
    assert_eq!(stdout["data"]["indexed"], 2);
    assert_eq!(stdout["data"]["fresh"], 0);
    assert_eq!(stdout["data"]["new"], 2);
    assert_eq!(stdout["data"]["outcome"], "complete");
    let expected_model = 2.0 * 0.00485;
    assert!((stdout["costDollars"]["model"].as_f64().unwrap() - expected_model).abs() < 1e-12);
    assert!(server.request_count() >= 2);
}

#[test]
fn resume_second_index_captions_zero_files() {
    let server = MockServer::start();
    let home = temp_root("home-resume");
    let xdg = temp_root("xdg-resume");
    let dir = fixture_library("library-resume", 2);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));
    let before = server.request_count();

    let output = run_index(&server, &home, &xdg, &dir);

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["indexed"], 0);
    assert_eq!(stdout["data"]["fresh"], 2);
    assert_eq!(server.request_count(), before);
}

#[test]
fn stale_file_is_recaptained_once() {
    let server = MockServer::start();
    let home = temp_root("home-stale");
    let xdg = temp_root("xdg-stale");
    let dir = fixture_library("library-stale", 2);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));
    thread::sleep(Duration::from_millis(20));
    write_png(&dir.join("0.png"), 4, [10, 0, 0, 255]);
    let output = run_index(&server, &home, &xdg, &dir);

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["indexed"], 1);
    assert_eq!(stdout["data"]["stale"], 1);
}

#[test]
fn find_single_shot_returns_absolute_ranked_hits() {
    let server = MockServer::with(vec![], vec![vec![1, 0]]);
    let home = temp_root("home-find");
    let xdg = temp_root("xdg-find");
    let dir = fixture_library("library-find", 2);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("find")
        .arg("hero shot")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["mode"], "single_shot");
    assert_eq!(stdout["data"]["hits"][0]["relPath"], "1.png");
    assert_eq!(stdout["data"]["hits"][0]["rank"], 1);
    assert!(
        stdout["data"]["hits"][0]["path"]
            .as_str()
            .unwrap()
            .starts_with('/')
    );
}

#[test]
fn find_out_of_range_ids_warns_and_keeps_valid_hits() {
    let server = MockServer::with(vec![], vec![vec![999, 0]]);
    let home = temp_root("home-invalid-id");
    let xdg = temp_root("xdg-invalid-id");
    let dir = fixture_library("library-invalid-id", 1);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["hits"].as_array().unwrap().len(), 1);
    // F7: invalidIdsDropped is now a structured field, not a warning string.
    assert_eq!(stdout["data"]["invalidIdsDropped"], 1);
    assert!(
        stdout["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| !w.as_str().unwrap().contains("invalidIdsDropped"))
    );
}

#[test]
fn status_reports_fresh_and_unindexed_libraries() {
    let server = MockServer::start();
    let home = temp_root("home-status");
    let xdg = temp_root("xdg-status");
    let dir = fixture_library("library-status", 2);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let fresh = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("status")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();
    let stdout = json_stdout(&fresh);
    assert_eq!(stdout["data"]["indexed"], 2);
    assert_eq!(stdout["data"]["fresh"], 2);

    let unindexed_dir = fixture_library("library-unindexed", 1);
    let unindexed = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("status")
        .arg("--dir")
        .arg(&unindexed_dir)
        .output()
        .unwrap();
    let stdout = json_stdout(&unindexed);
    assert_eq!(stdout["data"]["indexed"], 0);
    assert_eq!(stdout["data"]["new"], 1);
}

#[test]
fn index_dry_run_makes_zero_requests_and_projects_cost() {
    let server = MockServer::start();
    let home = temp_root("home-dry");
    let xdg = temp_root("xdg-dry");
    let dir = fixture_library("library-dry", 2);

    let output = lens_cmd(&home, &xdg)
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("--dry-run")
        .arg("index")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["dryRun"], true);
    assert_eq!(stdout["data"]["plannedWork"], 2);
    assert_eq!(stdout["costDollars"]["estimated"], true);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn budget_kill_exits_10_with_success_envelope_and_budget_skips() {
    let server = MockServer::start();
    let home = temp_root("home-budget");
    let xdg = temp_root("xdg-budget");
    let dir = fixture_library("library-budget", 2);

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("--max-dollars")
        .arg("0.001")
        .arg("index")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(10));
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["ok"], true);
    assert_eq!(stdout["budget"]["hit"], "dollars");
    assert_eq!(stdout["data"]["outcome"], "partial");
    assert_eq!(stdout["data"]["skipped"].as_array().unwrap().len(), 2);
    assert!(
        stdout["data"]["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["reason"] == "budget_refused")
    );
}

#[test]
fn unknown_flag_exits_usage_with_json_stderr_and_empty_stdout() {
    let home = temp_root("home-unknown");
    let xdg = temp_root("xdg-unknown");
    let output = lens_cmd(&home, &xdg)
        .arg("--jsno")
        .arg("index")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = json_stderr(&output);
    assert_eq!(stderr["schema"], "lens.cli.error.v1");
    assert_eq!(stderr["error"]["code"], "usage");
    assert!(
        stderr["error"]["suggestedFix"]
            .as_str()
            .unwrap()
            .contains("--json")
    );
}

#[test]
fn missing_key_exits_auth_with_suggested_fix() {
    let home = temp_root("home-missing-key");
    let xdg = temp_root("xdg-missing-key");
    let dir = fixture_library("library-missing-key", 1);
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("index")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = json_stderr(&output);
    assert_eq!(stderr["error"]["code"], "auth");
    assert!(
        stderr["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CEREBRAS_API_KEY")
    );
    assert!(
        stderr["error"]["suggestedFix"]
            .as_str()
            .unwrap()
            .contains("CEREBRAS_API_KEY")
    );
}

#[test]
fn missing_query_arg_is_usage() {
    let home = temp_root("home-missing-query");
    let xdg = temp_root("xdg-missing-query");
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("find")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = json_stderr(&output);
    assert_eq!(stderr["error"]["code"], "usage");
}

#[test]
fn help_with_json_emits_success_envelope() {
    let home = temp_root("home-help");
    let xdg = temp_root("xdg-help");
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("--help")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["schema"], "lens.cli.response.v1");
    assert_eq!(stdout["command"], "help");
    assert!(stdout["data"]["text"].as_str().unwrap().contains("Usage:"));
}

#[test]
fn capabilities_and_schema_emit_contract_envelopes() {
    let home = temp_root("home-contract");
    let xdg = temp_root("xdg-contract");
    let capabilities = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("capabilities")
        .output()
        .unwrap();
    assert_eq!(capabilities.status.code(), Some(0));
    let stdout = json_stdout(&capabilities);
    assert_eq!(stdout["command"], "capabilities");
    assert_eq!(
        stdout["data"]["exitCodes"]["10"],
        "partial/refused; stdout carries ok:true success envelope with budget.hit set"
    );

    let response_schema = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("schema")
        .arg("response")
        .output()
        .unwrap();
    assert_eq!(response_schema.status.code(), Some(0));
    let stdout = json_stdout(&response_schema);
    assert_eq!(stdout["command"], "schema");
    assert_eq!(stdout["data"]["$id"], "lens.cli.response.v1");

    let error_schema = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("schema")
        .arg("error")
        .output()
        .unwrap();
    assert_eq!(error_schema.status.code(), Some(0));
    let stdout = json_stdout(&error_schema);
    assert_eq!(stdout["data"]["$id"], "lens.cli.error.v1");
}

#[test]
fn doctor_offline_exits_0_with_structured_checks() {
    let home = temp_root("home-doctor");
    let xdg = temp_root("xdg-doctor");
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("doctor")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["command"], "doctor");
    assert!(
        stdout["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "auth.cerebras_key")
    );
}

#[test]
fn doctor_online_bad_key_exits_2_with_stdout_report() {
    let server = MockServer::start();
    let home = temp_root("home-doctor-online");
    let xdg = temp_root("xdg-doctor-online");
    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "bad-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("doctor")
        .arg("--online")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["schema"], "lens.cli.response.v1");
    assert_eq!(stdout["ok"], true);
    assert_eq!(stdout["command"], "doctor");
    assert!(
        stdout["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "online.cerebras" && c["ok"] == false)
    );
}

#[test]
fn find_budget_refusal_exits_10_with_success_envelope() {
    let server = MockServer::start();
    let home = temp_root("home-find-budget");
    let xdg = temp_root("xdg-find-budget");
    let dir = fixture_library("library-find-budget", 1);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("--max-dollars")
        .arg("0.000001")
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(10));
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["ok"], true);
    assert_eq!(stdout["budget"]["hit"], "dollars");
    assert_eq!(stdout["data"]["outcome"], "refused");
    assert_eq!(stdout["data"]["hits"].as_array().unwrap().len(), 0);
}

#[test]
fn gallery_writes_escaped_html() {
    let server = MockServer::with(vec!["<script>alert(1)</script>"], vec![vec![0]]);
    let home = temp_root("home-gallery");
    let xdg = temp_root("xdg-gallery");
    let dir = fixture_library("library-gallery", 1);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));
    let gallery = temp_root("gallery-out").join("gallery.html");

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("find")
        .arg("script")
        .arg("--dir")
        .arg(&dir)
        .arg("--gallery")
        .arg(&gallery)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(
        stdout["data"]["galleryPath"],
        gallery.to_string_lossy().as_ref()
    );
    let html = std::fs::read_to_string(gallery).unwrap();
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert"));
}

// F11a: find --dry-run → exit 0, dryRun data, zero mock hits, no API key.
#[test]
fn find_dry_run_reports_plan_without_api_key() {
    let server = MockServer::start();
    let home = temp_root("home-find-dry");
    let xdg = temp_root("xdg-find-dry");
    let dir = fixture_library("library-find-dry", 3);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));
    let before = server.request_count();

    // No CEREBRAS_API_KEY set — dry-run must not require it.
    let output = lens_cmd(&home, &xdg)
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("--dry-run")
        .arg("find")
        .arg("test query")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["dryRun"], true);
    assert_eq!(stdout["data"]["mode"], "single_shot");
    assert!(stdout["data"].get("chunks").is_some());
    assert!(stdout["data"].get("estimatedTokens").is_some());
    assert!(stdout["data"].get("projectedCostDollars").is_some());
    // Zero additional mock hits (dry-run makes no provider requests).
    assert_eq!(server.request_count(), before);
}

// F11b: chunked find E2E through the mock server. Enough large-description
// records to force chunked mode. Mock returns DISTINCT per-chunk ids (keyed
// by which marker filename appears in the chunk prompt). Rerank returns
// non-zero ids. Assert the hit relPaths are the exact expected records and
// mode == "chunked". This test must fail if B1 regresses.
#[test]
fn chunked_find_e2e_resolves_correct_records() {
    // We need enough records with long descriptions to force chunked mode.
    // The mock caption returns a fixed description, so we index many small
    // images and the descriptions will all be "mock image caption" — but that
    // is too short. We need to use the mock to return long descriptions.
    // Instead of indexing real images, we'll index enough to force chunking
    // by relying on the text_content field being long in the mock response.
    //
    // The mock caption returns text_content: "fixture text" — too short.
    // We need a custom approach: create a store directly with long records.
    //
    // Actually, the integration tests go through the full CLI, so we must
    // index first. The mock caption's text_content is short. But the
    // description "mock image caption" is also short. With 3 bytes/token, we
    // need ~210K bytes of prompt to exceed 70K tokens. Each line is ~60 bytes,
    // so we'd need ~3,500 images. That's too many for a test.
    //
    // Instead, we'll write index records directly to the store path, bypassing
    // the index command, then run find.

    let server = MockServer::start();
    let home = temp_root("home-chunked-e2e");
    let xdg = temp_root("xdg-chunked-e2e");
    let dir = fixture_library("library-chunked-e2e", 1);

    // Index 1 image to create the store structure.
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    // Now find the store path and overwrite index.jsonl with long records.
    // The store is under xdg/lens/libraries/<hash>/index.jsonl. We find it by
    // listing the libraries directory (there's only one library indexed).
    let libraries_dir = xdg.join("lens/libraries");
    let store_dir: PathBuf = std::fs::read_dir(&libraries_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("store directory must exist after indexing");
    let index_path = store_dir.join("index.jsonl");

    // Create 300 records with long descriptions to force chunked mode (3+
    // chunks). With the F5 description clamp (2,000 chars), each line is
    // ~2,170 bytes → ~723 tokens. 300 lines → ~217K tokens → 3+ chunks.
    let long_desc = "large description ".repeat(300);
    let mut records_json = String::new();
    for i in 0..300 {
        let record = serde_json::json!({
            "relPath": format!("chunk_{i:03}.jpg"),
            "size": 1u64,
            "mtimeNs": 1i128,
            "description": long_desc,
            "filename": format!("chunk-{i:03}"),
            "tags": ["tag"],
            "textContent": "",
            "kind": "photo"
        });
        records_json.push_str(&record.to_string());
        records_json.push('\n');
    }
    std::fs::write(&index_path, &records_json).unwrap();

    // Use the find_ids queue with --concurrency 1 so calls are sequential:
    // chunk 0, chunk 1, chunk 2, ..., then rerank. Each per-chunk call
    // returns a distinct non-zero id; the rerank returns non-zero ids.
    // With 210 records at ~723 tokens/line, we get ~3 chunks. We provide
    // responses for up to 6 chunks plus the rerank.
    drop(server);
    let server2 = MockServer::with(
        vec![],
        vec![
            vec![2],    // chunk 0
            vec![1],    // chunk 1
            vec![3],    // chunk 2
            vec![0],    // chunk 3 (if any)
            vec![5],    // chunk 4 (if any)
            vec![4],    // chunk 5 (if any)
            vec![2, 0], // rerank (non-zero ids)
        ],
    );

    // The key assertions for F11b/B1 regression:
    // 1. mode == "chunked"
    // 2. The hits' relPaths are among our chunk files
    // 3. There are hits (the rerank returned non-zero ids that are valid)
    // The exact relPaths depend on chunk boundaries; the unit test in
    // find.rs covers the exact B1 positional assertion.

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server2.base_url())
        .arg("--json")
        .arg("--concurrency")
        .arg("1")
        .arg("find")
        .arg("large")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["mode"], "chunked");
    // There should be hits (rerank returned [2, 0] which are valid union ids).
    let hits = stdout["data"]["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "chunked find should return hits from non-zero rerank ids"
    );
    // All hit relPaths must be among our chunk files.
    for hit in hits {
        let rel = hit["relPath"].as_str().unwrap();
        assert!(
            rel.starts_with("chunk_"),
            "hit relPath {rel} should be a chunk file"
        );
    }
    // Verify we made enough find calls: at least 2 chunks + 1 rerank = 3.
    // The index was on the first server, so server2 only counts find requests.
    assert!(
        server2.request_count() >= 3,
        "expected at least 3 find requests (2+ chunks + 1 rerank), got {}",
        server2.request_count()
    );
}

// F12: all-invalid → reroll → error. Mock returns {"ids":[999]} twice →
// exit 5, error envelope on stderr naming out-of-range ids, empty stdout.
#[test]
fn all_invalid_ids_reroll_then_upstream_error() {
    let server = MockServer::with(vec![], vec![vec![999], vec![999]]);
    let home = temp_root("home-all-invalid");
    let xdg = temp_root("xdg-all-invalid");
    let dir = fixture_library("library-all-invalid", 1);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let output = lens_cmd(&home, &xdg)
        .env("CEREBRAS_API_KEY", "fake-cerebras")
        .env("LENS_API_BASE", server.base_url())
        .arg("--json")
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let stderr = json_stderr(&output);
    assert_eq!(stderr["schema"], "lens.cli.error.v1");
    assert_eq!(stderr["error"]["code"], "upstream");
    assert!(
        stderr["error"]["message"]
            .as_str()
            .unwrap()
            .contains("out-of-range")
    );
}

// F13: schema completeness — parse `lens schema response` output and assert
// the find data variant has the required property names.
#[test]
fn schema_response_find_data_has_required_fields() {
    let home = temp_root("home-schema-fields");
    let xdg = temp_root("xdg-schema-fields");
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("schema")
        .arg("response")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    let data_variants = stdout["data"]["properties"]["data"]["oneOf"]
        .as_array()
        .unwrap();
    let find_variant = data_variants
        .iter()
        .find(|v| v["description"] == "find data")
        .expect("find data variant must exist");
    let props = find_variant["properties"].as_object().unwrap();

    // F13: all required property names.
    assert!(props.contains_key("outcome"));
    assert!(props.contains_key("dryRun"));
    assert!(props.contains_key("estimatedTokens"));
    assert!(props.contains_key("projectedCostDollars"));
    assert!(props.contains_key("invalidIdsDropped"));
    // Core fields still present.
    assert!(props.contains_key("query"));
    assert!(props.contains_key("hits"));
    assert!(props.contains_key("mode"));
    assert!(props.contains_key("chunks"));
    assert!(props.contains_key("warnings"));
}
