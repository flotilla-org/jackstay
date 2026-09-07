# Jackstay

Jackstay is a 0.x Rust transport library with a C ABI. Porthole is one host;
examples and offline tests must work without its checkout or daemon. The host
obtains desktop authorization and selects sources. Keep the PipeWire mechanism's
already-authorized connection handoff.

Before claiming a change complete, run:

```
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt --check
```

Also check the relevant native feature (`backend-macos` or `backend-linux`) and
run `scripts/smoke-viewer.sh` for viewer/ABI changes. Offline SDL uses
`SDL_VIDEODRIVER=dummy`; this exercises synthetic content and cannot prove desktop
capture. Hardware checks and porthole integration need separate live evidence.

If a live macOS capture operation lacks Accessibility or Screen Recording
permission, report BLOCKED and wait for the user to grant it. Do not bypass the
call or invent a substitute capture path to avoid permission requirements.

The API and C ABI may change at 0.x; update version checks and consumers together.
Keep Rust and C clients on the same implementation. No compatibility shims or
mandatory porthole authority model belong in the transport core.
