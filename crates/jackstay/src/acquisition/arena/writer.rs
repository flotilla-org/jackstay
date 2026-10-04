//! CPU reservation and trusted writer payload boundary.
use super::*;

/// An exclusive retired slot. Drop abandons it without publication.
/// Do not allow a delegate to touch this slot after commit or abandonment.
pub struct CpuReservation {
    pub(super) map: Arc<ResourceMap>,
    pub(super) index: usize,
    pub(super) scope: [u8; 16],
}

/// Instruction identifying a slot in one particular arena allocation.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WriterSlot {
    pub arena_scope: [u8; 16],
    pub generation: u64,
    pub slot: u32,
}

impl CpuReservation {
    pub fn slot(&self) -> WriterSlot {
        WriterSlot {
            arena_scope: self.scope,
            generation: self.map.generation,
            slot: self.index as u32,
        }
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: exclusive reservation owns this retired/unclaimed slot. Unsafe
        // exported writers must obey the caller's exclusion/completion protocol.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.map
                    .payload_storage
                    .as_ptr()
                    .add(self.map.layout.payload_offset(self.index))
                    .cast_mut(),
                self.map.layout.payload_capacity,
            )
        }
    }
}
impl Drop for CpuReservation {
    fn drop(&mut self) {
        self.map.reserved[self.index].store(false, SeqCst);
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WriterDescriptor {
    pub arena_scope: [u8; 16],
    pub generation: u64,
    pub map_len: u64,
    pub slot_capacity: u64,
    pub slots: u32,
}

/// Keeps an exported allocation charged. Retain until every escaped handle and
/// mapping is closed (child acknowledgement or verified process exit).
pub struct WriterExport {
    map: Arc<ResourceMap>,
    scope: [u8; 16],
}
impl WriterExport {
    pub fn descriptor(&self) -> WriterDescriptor {
        WriterDescriptor {
            arena_scope: self.scope,
            generation: self.map.generation,
            map_len: self.map.layout.payload_len as u64,
            slot_capacity: self.map.layout.payload_capacity as u64,
            slots: self.map.layout.resources as u32,
        }
    }
    /// Duplicate only the payload object for a trusted same-user delegate.
    ///
    /// # Safety
    /// Keep this export alive until all copies and mapped views of the returned
    /// object are closed. The delegate may write only the reserved slot it is
    /// told to, and must finish before commit/abandon or local slice access.
    pub unsafe fn duplicate_object(&self) -> Result<OwnedFd, ArenaError> {
        Ok(self.map.payload_storage.try_clone_fd()?)
    }
}

/// Writable payload mapping; contains no arena bookkeeping.
pub struct DelegatedWriter {
    storage: SharedMemorySegment,
    descriptor: WriterDescriptor,
}
impl DelegatedWriter {
    /// # Safety
    /// The descriptor/object must come from a conforming producer, whose export
    /// owner outlives this mapping. Honor its reserved-slot completion protocol.
    pub unsafe fn from_parts(descriptor: WriterDescriptor, object: OwnedFd) -> Result<Self, ArenaError> {
        let capacity = usize::try_from(descriptor.slot_capacity).map_err(|_| ArenaError::Mapping("writer capacity overflow"))?;
        let len = usize::try_from(descriptor.map_len).map_err(|_| ArenaError::Mapping("writer length overflow"))?;
        if descriptor.generation == 0
            || descriptor.slots == 0
            || capacity == 0
            || len > isize::MAX as usize
            || checked_mul(descriptor.slots as usize, capacity)? > len
        {
            return Err(ArenaError::Mapping("invalid writer layout"));
        }
        Ok(Self {
            storage: SharedMemorySegment::map_read_write(object, len)?,
            descriptor,
        })
    }
    /// # Safety
    /// Producer must currently reserve this slot for this delegate exclusively;
    /// no local view or consumer may access it. Finish all writes before replying
    /// to the producer. Scope/generation checks cannot prove live reservation.
    pub unsafe fn bytes_mut(&mut self, slot: WriterSlot) -> Result<&mut [u8], ArenaError> {
        if slot.arena_scope != self.descriptor.arena_scope
            || slot.generation != self.descriptor.generation
            || slot.slot >= self.descriptor.slots
        {
            return Err(ArenaError::Configuration("stale or foreign writer slot"));
        }
        let capacity = self.descriptor.slot_capacity as usize;
        // SAFETY: checked layout and slot bounds; exclusive writes are required
        // by this method's caller contract.
        Ok(unsafe { std::slice::from_raw_parts_mut(self.storage.as_ptr().add(slot.slot as usize * capacity).cast_mut(), capacity) })
    }
}

impl ArenaProducer {
    pub fn reserve(&mut self) -> Result<Option<CpuReservation>, ArenaError> {
        if self.native_resources {
            return Err(ArenaError::Configuration("CPU reservation requires CPU arena"));
        }
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.poll_cleanup()?;
        if self.cursor == MAX_GENERATION {
            self.stop();
            return Err(ArenaError::GenerationsExhausted);
        }
        let Some(map) = &self.resources else { return Ok(None) };
        let oldest = self.cursor.saturating_sub(map.layout.history as u64 - 1).max(1);
        for offset in 0..map.layout.resources {
            let index = (self.next_slot + offset) % map.layout.resources;
            if map.reserved[index].load(SeqCst) {
                continue;
            }
            let generation = map.state(index).load(SeqCst) >> 1;
            if generation != 0 && generation >= oldest {
                continue;
            }
            map.state(index).store(generation << 1, SeqCst);
            if generation != 0 && self.claims.values().any(|claims| claims.contains(generation)) {
                continue;
            }
            map.reserved[index].store(true, SeqCst);
            return Ok(Some(CpuReservation {
                map: Arc::clone(map),
                index,
                scope: self.control.scope,
            }));
        }
        Ok(None)
    }
    /// Publish completed pixels. Descriptor payload_len supplies their length.
    /// Error consumes/abandons the reservation; it never publishes stale storage.
    pub fn commit(&mut self, reservation: CpuReservation, mut descriptor: FrameDescriptor) -> Result<PublishOutcome, ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        if reservation.scope != self.control.scope || self.resources.as_ref().is_none_or(|map| !Arc::ptr_eq(map, &reservation.map)) {
            return Err(ArenaError::Configuration("stale or foreign CPU reservation"));
        }
        let map = &reservation.map;
        if descriptor.payload_len > map.layout.payload_capacity as u64 {
            return Err(ArenaError::PayloadTooLarge);
        }
        if self.cursor == MAX_GENERATION {
            self.stop();
            return Err(ArenaError::GenerationsExhausted);
        }
        let index = reservation.index;
        let cursor = self.cursor + 1;
        descriptor.cursor = cursor;
        descriptor.config_generation = map.generation;
        descriptor.slot_id = index as u32;
        descriptor.payload_offset = map.layout.payload_offset(index) as u64;
        // SAFETY: exclusive retired slot; delegate writes completed before call.
        unsafe {
            map.descriptor_ptr(index).write(descriptor);
        }
        map.state(index).store((cursor << 1) | 1, SeqCst);
        self.control.ring(cursor).store(index as u64 + 1, SeqCst);
        self.control.word(LATEST).store(cursor, SeqCst);
        self.cursor = cursor;
        self.next_slot = (index + 1) % map.layout.resources;
        for claims in self.claims.values() {
            claims.signal(wait::DATA)?;
        }
        Ok(PublishOutcome::Published { cursor })
    }
    pub fn abandon(&mut self, reservation: CpuReservation) -> Result<(), ArenaError> {
        if reservation.scope != self.control.scope {
            return Err(ArenaError::Configuration("foreign CPU reservation"));
        }
        drop(reservation);
        Ok(())
    }
    /// Export again after each reconfiguration. None means capacity paused.
    pub fn export_writer(&mut self) -> Result<Option<WriterExport>, ArenaError> {
        if self.native_resources {
            return Err(ArenaError::Configuration("writer export requires CPU arena"));
        }
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        if self.resources.as_ref().is_some_and(|map| map.layout.payload_capacity == 0) {
            return Err(ArenaError::Configuration("writer export requires nonzero CPU storage"));
        }
        Ok(self.resources.as_ref().map(|map| WriterExport {
            map: Arc::clone(map),
            scope: self.control.scope,
        }))
    }
}
