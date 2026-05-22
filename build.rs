//! Build script — only does anything on Windows.
//!
//! DuckDB's `bundled` C++ build calls into the Windows Restart Manager API
//! (`RmStartSession`, `RmEndSession`, `RmRegisterResources`, `RmGetList`) to
//! produce a friendlier "this file is locked by process X" message when a
//! parquet file is held open by another program. Those symbols live in
//! `Rstrtmgr.lib`, which `link.exe` doesn't auto-link on the MSVC target.
//!
//! Without this hint the Windows release build fails with `LNK2019: unresolved
//! external symbol RmStartSession`. On every other target this build script
//! is a no-op.

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target == "windows" {
        println!("cargo:rustc-link-lib=Rstrtmgr");
    }
}
