//! Reports the brotli-compressed size of a `wasm32-unknown-unknown` artifact.
//!
//! Run: `wasm-size <artifact.wasm> <budget-bytes> <label>`
//!
//! This is deliberately dumb: it reads a file, compresses it with brotli at quality 11,
//! verifies the round trip, and prints the raw/compressed sizes against a budget. The
//! *build* happens elsewhere (the Makefile/CI), because measuring needs the exact
//! release profile and a `wasm-bindgen` CLI whose version matches the crate's exactly.
//!
//! # Why the round trip is verified
//!
//! `brotli --quality=11 --best` is rejected ("quality already set") **while still
//! writing output**, so a measurement pipeline that ignores the exit status silently
//! reports a plausible, wrong number. This tool uses one quality spelling and never
//! pipes through a shell, then decompresses and byte-compares before reporting anything.
//!
//! Output is one line of JSON plus a human line, so a CI diff sees both.

use std::io::{Read, Write};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: wasm-size <artifact.wasm> <budget-bytes> <label>");
        return std::process::ExitCode::from(2);
    }
    let path = &args[0];
    let budget: u64 = match args[1].parse() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("error: budget must be a number of bytes, got {:?}", args[1]);
            return std::process::ExitCode::from(2);
        }
    };
    let label = &args[2];

    let raw = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("error: cannot read {path}: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    if raw.len() < 8 || &raw[..4] != b"\0asm" {
        eprintln!("error: {path} is not a wasm module (bad magic)");
        return std::process::ExitCode::from(2);
    }

    // brotli q11. Quality 11 is the number the budget is defined against; using a
    // different quality would silently change what "exceeded" means.
    let mut compressed = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 11, 22);
        if let Err(e) = writer.write_all(&raw) {
            eprintln!("error: brotli failed: {e}");
            return std::process::ExitCode::from(1);
        }
    }

    // Round trip: a compressor that wrote nothing would otherwise report a great number.
    let mut round_tripped = Vec::new();
    {
        let mut reader = brotli::Decompressor::new(&compressed[..], 4096);
        if let Err(e) = reader.read_to_end(&mut round_tripped) {
            eprintln!("error: brotli round trip failed to decompress: {e}");
            return std::process::ExitCode::from(1);
        }
    }
    if round_tripped != raw {
        eprintln!(
            "error: brotli round trip is not byte-identical ({} -> {} bytes)",
            raw.len(),
            round_tripped.len()
        );
        return std::process::ExitCode::from(1);
    }

    let compressed_len = compressed.len() as u64;
    let raw_len = raw.len() as u64;
    let delta = compressed_len as i64 - budget as i64;
    let status = if delta <= 0 { "within" } else { "EXCEEDED" };

    println!(
        "{{\"label\":{label:?},\"raw_bytes\":{raw_len},\"brotli_q11_bytes\":{compressed_len},\"budget_bytes\":{budget},\"delta_bytes\":{delta},\"status\":\"{status}\"}}"
    );
    println!(
        "{label}: {compressed_len} B brotli q11 ({} KiB) vs {budget} B budget ({} KiB) -> {status} by {:.1} KiB",
        compressed_len as f64 / 1024.0,
        budget as f64 / 1024.0,
        (delta as f64 / 1024.0).abs()
    );

    // Exit 0 even when over budget: this is a measuring tool, and the caller decides.
    // The status field is what CI gates on, so an over-budget artifact is reported
    // rather than hidden behind a nonzero exit that a script might swallow.
    std::process::ExitCode::SUCCESS
}
