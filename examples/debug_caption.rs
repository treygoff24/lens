//! Diagnosis helper: run normalize+caption on one file, print raw model output.
//! Usage: CEREBRAS_API_KEY=... cargo run --release --example debug_caption -- <image>

use lens::indexer::CaptionChat;
use lens::normalize::{NormalizeOutput, normalize_image};
use lens::providers::cerebras::{CerebrasClient, json_repair};
use lens::providers::new_spend;
use std::path::Path;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: debug_caption <image>");
    let ext = Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_string();
    let normalized = match normalize_image(Path::new(&path), &ext, false) {
        NormalizeOutput::Normalized(n) => n,
        NormalizeOutput::Skip { reason, detail } => {
            eprintln!("SKIP {reason:?}: {detail:?}");
            return;
        }
    };
    eprintln!(
        "normalized: {} bytes, {}x{}",
        normalized.jpeg_bytes.len(),
        normalized.width,
        normalized.height
    );
    let key = std::env::var("CEREBRAS_API_KEY").expect("CEREBRAS_API_KEY");
    let client = CerebrasClient::new(
        key,
        "https://api.cerebras.ai/v1".into(),
        "gemma-4-31b".into(),
    )
    .with_spend(new_spend());
    for attempt in 1..=3 {
        match client.caption_chat(&normalized.jpeg_bytes, attempt > 1) {
            Ok(resp) => {
                let repaired = json_repair(&resp.content);
                let parsed: Result<serde_json::Value, _> = serde_json::from_str(&repaired);
                eprintln!(
                    "attempt {attempt}: len {} | parse {:?}",
                    resp.content.len(),
                    parsed.as_ref().err()
                );
                if parsed.is_err() {
                    eprintln!(
                        "RAW HEAD: {:?}",
                        &resp.content.chars().take(150).collect::<String>()
                    );
                    eprintln!(
                        "RAW TAIL: {:?}",
                        &resp
                            .content
                            .chars()
                            .rev()
                            .take(150)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect::<String>()
                    );
                }
            }
            Err(err) => eprintln!("attempt {attempt}: ERROR {err}"),
        }
    }
}
