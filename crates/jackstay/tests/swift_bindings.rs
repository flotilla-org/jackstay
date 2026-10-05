// The existing macOS CI runs this through cargo test, including both CPU-only
// and native-backend builds. Isolate the nested library/example build from the
// running workspace tests and their selected backend features.
#[cfg(target_os = "macos")]
#[test]
fn swift_import_static_link_and_delegated_writer() {
    let target = tempfile::tempdir().expect("temporary Swift build directory");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/smoke-swift.sh");
    let status = std::process::Command::new("bash")
        .arg(script)
        .env("CARGO_TARGET_DIR", target.path())
        .status()
        .expect("run Swift smoke");
    assert!(status.success(), "Swift smoke failed: {status}");
}
