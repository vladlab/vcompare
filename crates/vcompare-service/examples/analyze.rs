//! Headless analysis: `cargo run --release -p vcompare-service --example analyze -- <mode> <file> [<distorted>] [-o out.json]`
//!
//! mode: levels | diff | vmaf. Writes the same cache the app writes (next to
//! the file unless `-o` is given) and prints the summary.

use std::sync::Arc;
use vcompare_service::{engine::cache, AnalyzeParams, Service, UiEvent};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let out = args.iter().position(|a| a == "-o").map(|i| {
        let o = args.get(i + 1).cloned().expect("-o needs a path");
        args.drain(i..=i + 1);
        o
    });
    if args.len() < 2 {
        eprintln!("usage: analyze <levels|diff|vmaf> <file> [<distorted>] [-o out.json]");
        std::process::exit(2);
    }
    let mode = args[0].clone();
    let reference = args[1].clone();
    let distorted = args.get(2).cloned();

    let log_path = out.unwrap_or_else(|| match mode.as_str() {
        "levels" => cache::default_levels_path(&reference),
        _ => cache::check_cache(&reference, distorted.as_deref().unwrap_or_default(), None)["path"]
            .as_str()
            .unwrap_or("analysis.vcompare.json")
            .to_string(),
    });

    let sink = |e: UiEvent| {
        if let UiEvent::Progress { current, total } = e {
            if total > 0 && (current % 2000 == 0 || current == total) {
                eprint!("\r{current}/{total}");
            }
        }
    };
    let data_dir = std::env::temp_dir().join("vcompare-cli");
    let svc = Service::new(data_dir, Arc::new(sink));
    let params = AnalyzeParams {
        reference,
        distorted,
        mode: Some(mode),
        log_path: Some(log_path.clone()),
        force: Some(true),
        ..Default::default()
    };
    match svc.analyze(&params) {
        Ok(summary) => {
            eprintln!();
            println!("{}", serde_json::to_string_pretty(&summary).unwrap_or_default());
            eprintln!("saved: {log_path}");
        }
        Err(e) => {
            eprintln!("\nerror: {e}");
            std::process::exit(1);
        }
    }
}
