fn main() {
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    let feature = std::env::var("CARGO_FEATURE_BACKEND_MACOS").is_ok();
    if macos && feature {
        build_vt_shim();
    }
}

#[cfg(feature = "backend-macos")]
fn build_vt_shim() {
    println!("cargo:rerun-if-changed=src/vt_shim.m");
    cc::Build::new()
        .file("src/vt_shim.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=13.0")
        .compile("jackstay_bridge_vt");
    for framework in [
        "Foundation",
        "CoreFoundation",
        "CoreMedia",
        "CoreVideo",
        "VideoToolbox",
        "IOSurface",
        "Metal",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

#[cfg(not(feature = "backend-macos"))]
fn build_vt_shim() {}
