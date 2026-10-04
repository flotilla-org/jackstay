# In-place CPU publication and trusted delegated writers

## Ownership and slot states

The producer remains the sole admission, retirement, descriptor and publication
owner. A CPU slot moves from unpublished/retired to reserved, then published,
then retired. Reservation uses the existing sequentially consistent retirement
and claim scan: no slot inside retained history or held by a consumer is handed
out. Reservations are exclusive, identified by arena scope, allocation generation,
slot and a single-use owned reservation object. Commit validates allocation
identity and scope before publishing metadata, without copying pixels. Abandon returns the
reservation without publishing. Dropping a Rust reservation abandons it. Local
mutable views borrow the reservation and cannot outlive commit.

Capacity exhaustion returns a dropped/no-slot outcome, as copy-in publication
does. CPU arenas with zero payload capacity reject both reservation and writer
export; native-only arenas keep their existing publication path. Copy-in and native publication must skip reserved slots. Descriptor payload
length is validated against slot capacity; arena-owned cursor, slot, generation
and offset fields are stamped at commit. Delegate completion is a host protocol:
the producer must wait for its child to finish writing before committing or
abandoning, and the child must stop touching that slot afterwards.

## Payload export and trust

Split payload bytes into a separate shared-memory object from resource records.
Consumer grants carry this additional payload object, and replacement grants
carry both resource and payload objects. Payload offsets are relative to the
payload mapping. Allocation accounting includes both objects. This changes the
internal arena protocol version and requires updating setup and C imports.

Writer exports contain only the payload object plus immutable layout metadata:
arena scope, allocation generation, mapping length, slot count and slot capacity.
They never contain resource records, persistent control, claims, admission or
notification objects. The writer maps payload read/write and accepts only slot
instructions matching that export's scope and generation and within bounds.
Mapping/import and writes are unsafe Rust operations because remote protocol
compliance cannot be proved locally. A trusted same-user child can corrupt pixel
bytes; it cannot change consumer claims, resource generations, descriptors or
admission. Consumer safety depends on the child honoring the assigned-slot and
completion protocol. In particular, never provide safe mutable Rust references
that could alias a concurrently executing delegate.

## Reconfiguration and stale writers

Reconfiguration retires the old allocation and invalidates its reservations for
commit. Existing reservations retain their backing until dropped; retired
allocation bookkeeping must include them. Reserve and export return no object
while replacement is capacity-paused. Each installed replacement has a new
allocation generation and requires a fresh export. Old handles always refer to
the old object, never the replacement, so stale writes cannot affect current
frames. A mismatched-generation slot instruction is refused by the writer API.
The host closes old exported handles and obtains child acknowledgement of unmap
before returning their allocation charge. An explicit writer export lifetime
owner tracks escaped mappings; exporting raw handles is unsafe and requires the
host to retain that owner until acknowledgement or verified child exit.

## Windows equivalent

The payload object is a pagefile-backed file mapping. The setup host duplicates
only its payload handle into the verified child with DuplicateHandle; the child
uses MapViewOfFile with read/write access. Scope/generation/layout checks and
reservation rules are identical to Unix fd transfer. Handles are owned and closed
by the receiving process. No control or claim handle is exported to the writer.

## Public surfaces and validation

Expose reserve/commit/abandon and a writer export on ArenaProducer; toolkit source
handles expose the same arena operations for callers that fill slots locally or
coordinate a child. Rust ownership and opaque C handles make commit/abandon single-use without a
replayable public reservation serial. Native-only pools use a duplicate record
object for the unused sixth setup position; they allocate no payload object.

C gets opaque reservation and export owners plus explicit
layout and OS-object transfer functions, with an ABI minor bump beyond 0.12
(which already includes #72). Preserve the copy-in convenience API.

Tests cover leased-slot exclusion, abandoned and duplicate/stale reservations,
capacity-paused reserve/export, byte-exact subprocess writes, generation mismatch
and harmless old mappings, C parity, and toolkit publication. Native pools keep
the existing publication path and do not expose CPU writer storage.
