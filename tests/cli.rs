mod common;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use assert_cmd::Command;
use image::{ImageBuffer, Rgba};
use serde_json::Value;

use common::MockServer;
use lens::store::IndexRecord;

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

fn online_lens_cmd(server: &MockServer, home: &Path, xdg: &Path, api_key: &str) -> Command {
    let mut cmd = lens_cmd(home, xdg);
    cmd.env("CEREBRAS_API_KEY", api_key)
        .env("LENS_API_BASE", server.base_url())
        .arg("--json");
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

fn assert_exit(output: &std::process::Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_index_records(index_dir: &Path, records: &[IndexRecord]) {
    std::fs::create_dir_all(index_dir).unwrap();
    let mut jsonl = String::new();
    for record in records {
        jsonl.push_str(&serde_json::to_string(record).unwrap());
        jsonl.push('\n');
    }
    std::fs::write(index_dir.join("index.jsonl"), jsonl).unwrap();
}

fn index_record(rel_path: &str, kind: &str, description: &str) -> IndexRecord {
    IndexRecord {
        rel_path: rel_path.to_string(),
        size: 1,
        mtime_ns: 1,
        description: description.to_string(),
        filename: rel_path.to_string(),
        tags: vec![kind.to_string()],
        text_content: String::new(),
        kind: kind.to_string(),
    }
}

fn run_index(server: &MockServer, home: &Path, xdg: &Path, dir: &Path) -> std::process::Output {
    online_lens_cmd(server, home, xdg, "fake-cerebras")
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

    assert_exit(&output, 0);
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

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("find")
        .arg("hero shot")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_exit(&output, 0);
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

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["hits"].as_array().unwrap().len(), 1);
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

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
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
fn whitespace_query_exits_11_no_input() {
    let home = temp_root("home-blank-query");
    let xdg = temp_root("xdg-blank-query");
    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("find")
        .arg("   ")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(11));
    assert!(output.stdout.is_empty());
    let stderr = json_stderr(&output);
    assert_eq!(stderr["schema"], "lens.cli.error.v1");
    assert_eq!(stderr["error"]["code"], "no_input");
    assert!(
        stderr["error"]["suggestedFix"]
            .as_str()
            .unwrap()
            .contains("lens find")
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
    // Keep the published cost expectation tied to the measured acceptance run.
    assert!(
        stdout["data"]["costExpectations"]["prototypeRun"]
            .as_str()
            .unwrap()
            .contains("93s"),
        "prototypeRun should contain '93s', got: {}",
        stdout["data"]["costExpectations"]["prototypeRun"]
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
    let output = online_lens_cmd(&server, &home, &xdg, "bad-cerebras")
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

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
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

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
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

#[test]
fn find_dry_run_reports_plan_without_api_key() {
    let server = MockServer::start();
    let home = temp_root("home-find-dry");
    let xdg = temp_root("xdg-find-dry");
    let dir = fixture_library("library-find-dry", 3);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));
    let before = server.request_count();

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

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["dryRun"], true);
    assert_eq!(stdout["data"]["mode"], "single_shot");
    assert!(stdout["data"].get("chunks").is_some());
    assert!(stdout["data"].get("estimatedTokens").is_some());
    assert!(stdout["data"].get("projectedCostDollars").is_some());
    assert_eq!(server.request_count(), before);
}

#[test]
fn chunked_find_e2e_resolves_correct_records() {
    let server = MockServer::start();
    let home = temp_root("home-chunked-e2e");
    let xdg = temp_root("xdg-chunked-e2e");
    let dir = fixture_library("library-chunked-e2e", 1);

    // Let the CLI create its hashed store path before replacing the rows.
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let libraries_dir = xdg.join("lens/libraries");
    let store_dir: PathBuf = std::fs::read_dir(&libraries_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .expect("store directory must exist after indexing");
    let index_path = store_dir.join("index.jsonl");

    // 300 clamped rows force at least three chunks.
    let long_desc = "large description ".repeat(300);
    let mut records_json = String::new();
    for i in 0..300 {
        let record = IndexRecord {
            rel_path: format!("chunk_{i:03}.jpg"),
            size: 1,
            mtime_ns: 1,
            description: long_desc.clone(),
            filename: format!("chunk-{i:03}"),
            tags: vec!["tag".into()],
            text_content: String::new(),
            kind: "photo".into(),
        };
        records_json.push_str(&serde_json::to_string(&record).unwrap());
        records_json.push('\n');
    }
    std::fs::write(&index_path, &records_json).unwrap();

    // Serial calls map queued responses deterministically to chunks, then rerank.
    drop(server);
    let server2 = MockServer::with(
        vec![],
        vec![
            vec![2],
            vec![1],
            vec![3],
            vec![0],
            vec![5],
            vec![4],
            vec![2, 0], // Nonzero rerank ids catch resolution against the full index.
        ],
    );

    let output = online_lens_cmd(&server2, &home, &xdg, "fake-cerebras")
        .arg("--concurrency")
        .arg("1")
        .arg("find")
        .arg("large")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["mode"], "chunked");
    let hits = stdout["data"]["hits"].as_array().unwrap();
    assert!(
        !hits.is_empty(),
        "chunked find should return hits from non-zero rerank ids"
    );
    for hit in hits {
        let rel = hit["relPath"].as_str().unwrap();
        assert!(
            rel.starts_with("chunk_"),
            "hit relPath {rel} should be a chunk file"
        );
    }
    // Indexing used server1, so every server2 request is a find call.
    assert!(
        server2.request_count() >= 3,
        "expected at least 3 find requests (2+ chunks + 1 rerank), got {}",
        server2.request_count()
    );
}

#[test]
fn all_invalid_ids_reroll_then_upstream_error() {
    let server = MockServer::with(vec![], vec![vec![999], vec![999]]);
    let home = temp_root("home-all-invalid");
    let xdg = temp_root("xdg-all-invalid");
    let dir = fixture_library("library-all-invalid", 1);
    assert_eq!(run_index(&server, &home, &xdg, &dir).status.code(), Some(0));

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
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

#[test]
fn find_kind_filters_records_before_model_call() {
    let server = MockServer::with(vec![], vec![vec![0]]);
    let home = temp_root("home-find-kind");
    let xdg = temp_root("xdg-find-kind");
    let dir = temp_root("library-find-kind");
    let index_dir = temp_root("index-find-kind");
    // Distinct markers make pre-model filtering observable in the request body.
    write_index_records(
        &index_dir,
        &[
            index_record("a-graphic.png", "graphic", "GRAPHIC_MARKER_BETA"),
            index_record("z-photo.png", "photo", "PHOTO_MARKER_ALPHA"),
        ],
    );

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("photo")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("photo")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["kindFilter"], serde_json::json!(["photo"]));
    assert_eq!(stdout["data"]["searched"], 1);
    assert_eq!(stdout["data"]["hits"][0]["relPath"], "z-photo.png");
    assert_eq!(stdout["data"]["hits"][0]["kind"], "photo");
    assert!(server.request_count() >= 1);

    let body = server
        .last_request_body()
        .expect("at least one request must have reached the mock");
    assert!(
        body.contains("PHOTO_MARKER_ALPHA"),
        "request body must contain the photo record's description marker"
    );
    assert!(
        !body.contains("GRAPHIC_MARKER_BETA"),
        "request body must NOT contain the graphic record's description marker"
    );
}

#[test]
fn find_kind_zero_match_returns_empty_success_without_model_call() {
    let server = MockServer::start();
    let home = temp_root("home-find-kind-zero");
    let xdg = temp_root("xdg-find-kind-zero");
    let dir = temp_root("library-find-kind-zero");
    let index_dir = temp_root("index-find-kind-zero");
    write_index_records(
        &index_dir,
        &[
            index_record("photo.png", "photo", "photo line"),
            index_record("graphic.png", "graphic", "graphic line"),
        ],
    );

    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("nosuchkind")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["outcome"], "answered");
    assert_eq!(stdout["data"]["hits"].as_array().unwrap().len(), 0);
    assert_eq!(stdout["data"]["searched"], 0);
    assert_eq!(
        stdout["data"]["warnings"],
        serde_json::json!(["kind filter matched 0 of 2 records; kinds present: graphic,photo"])
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn find_dry_run_kind_filter_reduces_token_estimate_without_network() {
    let server = MockServer::start();
    let home = temp_root("home-find-kind-dry");
    let xdg = temp_root("xdg-find-kind-dry");
    let dir = temp_root("library-find-kind-dry");
    let index_dir = temp_root("index-find-kind-dry");
    let long_photo = "photo description ".repeat(80);
    let long_graphic = "graphic description ".repeat(80);
    write_index_records(
        &index_dir,
        &[
            index_record("photo.png", "photo", &long_photo),
            index_record("graphic.png", "graphic", &long_graphic),
        ],
    );

    let all = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("--dry-run")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .output()
        .unwrap();
    let filtered = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("--dry-run")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("photo")
        .output()
        .unwrap();

    assert_eq!(all.status.code(), Some(0));
    assert_eq!(filtered.status.code(), Some(0));
    let all = json_stdout(&all);
    let filtered = json_stdout(&filtered);
    assert_eq!(filtered["data"]["kindFilter"], serde_json::json!(["photo"]));
    assert_eq!(filtered["data"]["searched"], 1);
    assert!(
        filtered["data"]["estimatedTokens"].as_u64().unwrap()
            < all["data"]["estimatedTokens"].as_u64().unwrap()
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn find_kind_filter_is_case_insensitive() {
    let server = MockServer::with(vec![], vec![vec![0]]);
    let home = temp_root("home-find-kind-case");
    let xdg = temp_root("xdg-find-kind-case");
    let dir = temp_root("library-find-kind-case");
    let index_dir = temp_root("index-find-kind-case");
    write_index_records(
        &index_dir,
        &[index_record("photo.png", "photo", "photo line")],
    );

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("photo")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("PHOTO")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["kindFilter"], serde_json::json!(["photo"]));
    assert_eq!(stdout["data"]["searched"], 1);
    assert_eq!(stdout["data"]["hits"][0]["relPath"], "photo.png");
}

#[test]
fn find_kind_duplicate_filters_are_deduped_in_echo() {
    let server = MockServer::with(vec![], vec![vec![0]]);
    let home = temp_root("home-find-kind-dedup");
    let xdg = temp_root("xdg-find-kind-dedup");
    let dir = temp_root("library-find-kind-dedup");
    let index_dir = temp_root("index-find-kind-dedup");
    write_index_records(
        &index_dir,
        &[
            index_record("photo-0.png", "photo", "photo line zero"),
            index_record("graphic-0.png", "graphic", "graphic line zero"),
        ],
    );

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("photo")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("PHOTO,photo")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = json_stdout(&output);
    assert_eq!(stdout["data"]["kindFilter"], serde_json::json!(["photo"]));
    assert_eq!(stdout["data"]["searched"], 1);
}

#[test]
fn find_kind_budget_refused_echoes_filtered_count_without_model_call() {
    let server = MockServer::start();
    let home = temp_root("home-find-kind-budget");
    let xdg = temp_root("xdg-find-kind-budget");
    let dir = temp_root("library-find-kind-budget");
    let index_dir = temp_root("index-find-kind-budget");
    write_index_records(
        &index_dir,
        &[
            index_record("photo-0.png", "photo", "photo desc zero"),
            index_record("photo-1.png", "photo", "photo desc one"),
            index_record("graphic-0.png", "graphic", "graphic desc zero"),
            index_record("graphic-1.png", "graphic", "graphic desc one"),
        ],
    );
    let before = server.request_count();

    let output = online_lens_cmd(&server, &home, &xdg, "fake-cerebras")
        .arg("--max-dollars")
        .arg("0.000001")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("photo")
        .output()
        .unwrap();

    assert_exit(&output, 10);
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(stdout["ok"], true);
    assert_eq!(stdout["budget"]["hit"], "dollars");
    assert_eq!(stdout["data"]["outcome"], "refused");
    assert_eq!(stdout["data"]["kindFilter"], serde_json::json!(["photo"]));
    assert_eq!(stdout["data"]["searched"], 2);
    assert_eq!(stdout["data"]["hits"].as_array().unwrap().len(), 0);
    assert_eq!(server.request_count(), before);
}

#[test]
fn find_dry_run_zero_kind_match_no_api_key() {
    let server = MockServer::start();
    let home = temp_root("home-find-dry-zero-kind");
    let xdg = temp_root("xdg-find-dry-zero-kind");
    let dir = temp_root("library-find-dry-zero-kind");
    let index_dir = temp_root("index-find-dry-zero-kind");
    write_index_records(
        &index_dir,
        &[
            index_record("photo.png", "photo", "photo line"),
            index_record("graphic.png", "graphic", "graphic line"),
        ],
    );
    let before = server.request_count();

    let output = lens_cmd(&home, &xdg)
        .arg("--json")
        .arg("--dry-run")
        .arg("--index-path")
        .arg(&index_dir)
        .arg("find")
        .arg("anything")
        .arg("--dir")
        .arg(&dir)
        .arg("--kind")
        .arg("nosuchkind")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty());
    let stdout = json_stdout(&output);
    assert_eq!(
        stdout["data"]["kindFilter"],
        serde_json::json!(["nosuchkind"])
    );
    assert_eq!(stdout["data"]["searched"], 0);
    let warnings = stdout["data"]["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|w| w.as_str().unwrap()
            == "kind filter matched 0 of 2 records; kinds present: graphic,photo"),
        "warnings should contain the exact zero-match string, got: {warnings:?}"
    );
    assert_eq!(server.request_count(), before);
}
