fn main() {
    // The macOS native backend shim: compiled only when the feature is on
    // AND the target is macOS (the feature may be enabled by a dependent on
    // any platform; it must be a no-op elsewhere).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let backend_macos = std::env::var_os("CARGO_FEATURE_BACKEND_MACOS").is_some();
    if target_os == "macos" && backend_macos {
        build_macos_shim();
    }
}

#[cfg(feature = "backend-macos")]
fn build_macos_shim() {
    println!("cargo:rerun-if-changed=src/native/macos_shim.m");
    println!("cargo:rerun-if-changed=src/native/macos_xpc_shim.m");
    cc::Build::new()
        .file("src/native/macos_shim.m")
        .file("src/native/macos_xpc_shim.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=13.0")
        .compile("porthole_native_macos");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Metal");
    println!("cargo:rustc-link-lib=framework=IOSurface");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}

#[cfg(not(feature = "backend-macos"))]
fn build_macos_shim() {
    unreachable!("CARGO_FEATURE_BACKEND_MACOS implies the feature is enabled");
}
