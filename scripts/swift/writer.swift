import Foundation
import Jackstay

struct WriterDescriptor: Decodable {
    let arena_scope: [UInt8]
    let generation: UInt64
    let map_len: UInt64
    let slot_capacity: UInt64
    let slots: UInt32
}

struct WriterSlot: Decodable {
    let arena_scope: [UInt8]
    let generation: UInt64
    let slot: UInt32
}

struct WriterOffer: Decodable {
    let descriptor: WriterDescriptor
    let slot: WriterSlot
    let object: Int32
}

func check(_ status: ft_status, _ operation: String) {
    precondition(status == FT_STATUS_OK, "\(operation): status \(status)")
}

// All public headers are part of the module; these types also check the ring,
// input, bootstrap and affordance declarations beyond the CPU writer surface.
precondition(MemoryLayout<jackstay_ring_header>.size > 0)
precondition(MemoryLayout<ft_input_event>.size > 0)
precondition(MemoryLayout<ft_aff_snapshot>.size > 0)
precondition(FT_BOOTSTRAP_INPUT_NONE == 0)
// Clang's Swift importer cannot expose the C cast in FT_ABI_VERSION. Build the
// same value from its imported components, without changing the public C ABI.
let FT_ABI_VERSION = UInt32((FT_ABI_VERSION_MAJOR << 16) | FT_ABI_VERSION_MINOR)
precondition(ft_abi_version() == FT_ABI_VERSION, "header/library ABI mismatch")

let offer = try JSONDecoder().decode(
    WriterOffer.self, from: FileHandle.standardInput.readDataToEndOfFile())
precondition(offer.descriptor.arena_scope.count == 16 && offer.slot.arena_scope.count == 16)
var descriptor = ft_cpu_writer_descriptor()
withUnsafeMutableBytes(of: &descriptor.arena_scope) { $0.copyBytes(from: offer.descriptor.arena_scope) }
descriptor.generation = offer.descriptor.generation
descriptor.map_len = offer.descriptor.map_len
descriptor.slot_capacity = offer.descriptor.slot_capacity
descriptor.slots = offer.descriptor.slots
var slot = ft_cpu_writer_slot()
withUnsafeMutableBytes(of: &slot.arena_scope) { $0.copyBytes(from: offer.slot.arena_scope) }
slot.generation = offer.slot.generation
slot.slot = offer.slot.slot
var object = offer.object
var writer: OpaquePointer?
check(ft_cpu_writer_import(&descriptor, &object, &writer), "writer import")
precondition(object == FT_OS_OBJECT_NONE && writer != nil, "import must consume the object")
var bytes: UnsafeMutablePointer<UInt8>?
var length = 0
check(ft_cpu_writer_slot_view(writer, &slot, &bytes, &length), "slot view")
precondition(length == 4 && bytes != nil, "expected four-byte reserved slot")
bytes!.update(from: [UInt8]("Swif".utf8), count: length)
check(ft_cpu_writer_destroy(&writer), "writer destroy")
precondition(writer == nil, "destroy must clear the writer")
print("Swift import Jackstay: ABI and delegated writer passed")
