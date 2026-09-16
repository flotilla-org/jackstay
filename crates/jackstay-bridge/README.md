# jackstay-bridge

The cross-host bridge: an egress half that encodes a local Jackstay
publication and sends it over a link, and an ingress half that decodes what
arrives and republishes it. See `docs/design/bridge.md`.

```sh
cargo build -p jackstay-bridge --features backend-macos
target/debug/jackstay-bridge probe                 # capability probe as JSON
target/debug/jackstay-bridge loopback --frames 120 # both halves in one process, self-verifying
target/debug/jackstay-bridge loopback --viewer-service work.flotilla.jackstay.bridge.demo --viewer-token demo
build/viewer/capture-viewer-sdl --native --mach-service work.flotilla.jackstay.bridge.demo --token demo
```

The halves as processes, for a coordinator that forwarded two sockets:

```sh
jackstay-bridge egress  --media M --control C --service SOURCE --source-token T --link-token L
jackstay-bridge ingress --media M --control C --service NAME   --viewer-token V --link-token L
```

Without `--features backend-macos` only the wire, clock and policy code builds.
