# Cross-host bridge implementation

The `jackstay-bridge` implementation, example, tests, and design note have moved
to [Porthole](https://github.com/flotilla-org/porthole/tree/main/crates/jackstay-bridge).
The [design note](https://github.com/flotilla-org/porthole/blob/main/docs/jackstay-bridge.md)
describes the wire format, codec path, and measured behaviour.

Porthole builds and bundles the bridge and depends on Jackstay for transport
and graph primitives. Jackstay remains usable without Porthole.
