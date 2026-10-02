fn main() {
    // A `cargo test` binary on Windows dies at load with
    // STATUS_ENTRYPOINT_NOT_FOUND: tauri-build embeds its app manifest as a
    // resource of the APP binary only, so the test binary loads the old
    // ComCtl32 v5, which lacks the dialog entry points (TaskDialogIndirect).
    // Embed the same manifest through the linker instead, which covers every
    // binary this crate links. Same workaround as tauri's own build.rs
    // (`embed_manifest_for_tests`).
    let windows_msvc = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let mut windows = tauri_build::WindowsAttributes::new();
    if windows_msvc {
        let manifest = std::env::current_dir()
            .unwrap()
            .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
        windows = tauri_build::WindowsAttributes::new_without_app_manifest();
    }
    tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
        .expect("failed to run tauri-build");
}
