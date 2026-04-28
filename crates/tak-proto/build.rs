//! Build script: regenerate Rust types from vendored `.proto` files in `proto/`.
//! See `UPSTREAM.md` for the source of truth.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let proto_dir = PathBuf::from("proto");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let protos: Vec<PathBuf> = std::fs::read_dir(&proto_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "proto"))
        .collect();

    // Use vendored protoc so build is hermetic.
    // SAFETY: build scripts are single-threaded; setting env here is sound.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    }

    let mut config = prost_build::Config::new();
    config.out_dir(&out_dir);
    config.bytes(["."]);   // map `bytes` proto fields to bytes::Bytes (zero-copy)
    config.compile_protos(&protos, &[proto_dir.clone()])?;

    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
