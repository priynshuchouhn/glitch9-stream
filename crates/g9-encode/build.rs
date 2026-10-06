//! Build script: on Windows, generate NVENC FFI bindings from the vendored
//! `nvEncodeAPI.h` with bindgen. This replaces the previous hand-written FFI so
//! struct layouts/sizes are byte-exact to the SDK header. On non-Windows it is a
//! no-op (the crate's Windows code is `#[cfg(windows)]`).

fn main() {
    // Only generate on Windows targets; nvEncodeAPI.h needs <windows.h> (GUID/HANDLE).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    #[cfg(windows)]
    {
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
        let out_path = std::path::Path::new(&out_dir).join("nvenc_bindings.rs");

        println!("cargo:rerun-if-changed=vendor/nvEncodeAPI.h");
        println!("cargo:rerun-if-changed=vendor/wrapper.h");

        let bindings = bindgen::Builder::default()
            .header("vendor/wrapper.h")
            // Only emit NVENC symbols, not all of windows.h.
            .allowlist_type("NV_ENC.*")
            .allowlist_type("NVENC.*")
            .allowlist_type("_NV_ENC.*")
            .allowlist_type("PNVENC.*")
            .allowlist_function("NvEncodeAPI.*")
            .allowlist_var("NV_ENC.*")
            .allowlist_var("NVENC.*")
            // GUIDs are `const GUID` objects in the header; keep them.
            .allowlist_var(".*_GUID")
            // Let bindgen emit its own GUID type (from windows.h) so the const GUID
            // objects in the header resolve with the correct layout. We do not alias
            // to windows::core::GUID to avoid type mismatches.
            .allowlist_type("_GUID")
            .allowlist_type("GUID")
            // Reasonable defaults.
            .derive_default(true)
            .derive_copy(true)
            .layout_tests(false)
            .generate()
            .expect("bindgen failed to generate NVENC bindings");

        bindings
            .write_to_file(&out_path)
            .expect("failed to write nvenc_bindings.rs");
    }
}
