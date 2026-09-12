---
status: accepted
---
# Frame leases outlive publication-ring entries

A live CPU viewer failure exposed a lifetime mismatch: the daemon had granted a
frame lease, but the consumer treated expiration of the corresponding diagnostic
ring entry as a fatal error. We will implement shared-memory acquisition with a
stable descriptor and retained storage after acquisition succeeds. Ring reuse
must not invalidate that lease.

Consumers negotiate holding reservations at admission. Admission preserves
existing reservations, retained history, and producer working capacity. Deferred
GPU releases remain charged until completion. Both latest and ordered delivery
use this ownership contract; ordered delivery reports gaps and does not imply
lossless capture.

We considered repairing only socket-mediated selection and leasing. That remains
a valid transitional implementation, but the agreed scope delivers shared-memory
acquisition, efficient waits, cleanup, and reconfiguration, demonstrated by a CPU
path and an existing GPU path. Authorization, connection setup, and handle
transfer remain control-channel responsibilities.

See the [contract and implementation slices](../specs/acquisition-lifetime-contract.md).
