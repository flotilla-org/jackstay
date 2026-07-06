//! Synthetic Linux dmabuf producer backend.
//!
//! This is the first real Linux [`NativeFrameBackend`] implementation shape:
//! it allocates dmabuf pool slots from dma-heap, exports them through the
//! Linux attach transport, and publishes a DRM syncobj timeline fence. It does
//! not yet perform PipeWire capture or GPU copies into those surfaces.

use std::{
    collections::HashMap,
    os::fd::{AsRawFd, OwnedFd},
    path::Path,
};

use crate::{
    error::{CaptureTransferError, Result},
    model::{PayloadKind, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate,
        lease::{NativeLeaseBook, NativeLeaseIdentity, NativeLeaseRelease},
        linux::{
            LinuxDmabufPlaneHandle, LinuxNativeLeaseBackend, LinuxSurfaceHandle, LinuxSyncDescriptor, LinuxSyncHandle,
            dmabuf::{DmaBuf, DmaHeap, FT_NATIVE_HANDLE_DMABUF},
            drm::{DrmDevice, DrmSyncobjTimeline, FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE},
        },
    },
};

/// A captured frame placeholder for the synthetic backend. Real PipeWire /
/// Vulkan staging will replace this with source-frame state.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyntheticLinuxFrame;

#[derive(Debug)]
pub struct SyntheticLinuxBackend {
    drm: DrmDevice,
    heap: DmaHeap,
    next_pool_id: u64,
    next_fence_id: u64,
    lease_book: NativeLeaseBook,
    release_syncs: HashMap<u64, DrmSyncobjTimeline>,
}

impl SyntheticLinuxBackend {
    pub fn open(drm_render_path: impl AsRef<Path>, dma_heap_path: impl AsRef<Path>) -> Result<Self> {
        let drm = DrmDevice::open(drm_render_path)?;
        if !drm.supports_syncobj_timeline()? {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-drm-syncobj-timeline-cap",
                message: "DRM device does not support syncobj timelines".to_string(),
            });
        }
        Ok(Self {
            drm,
            heap: DmaHeap::open(dma_heap_path)?,
            next_pool_id: 1,
            next_fence_id: 1,
            lease_book: NativeLeaseBook::new(),
            release_syncs: HashMap::new(),
        })
    }

    fn refresh_release_syncs(&mut self) -> Result<()> {
        for (release_sync_id, timeline) in &self.release_syncs {
            let reached_value = timeline.query(0)?;
            self.lease_book
                .complete_release_sync(*release_sync_id, reached_value)
                .map_err(CaptureTransferError::from)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct SyntheticLinuxPool {
    pool_id: u64,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    modifier: u64,
    stride: u32,
    surfaces: Vec<DmaBuf>,
    reuse_blocked: Vec<bool>,
}

impl SyntheticLinuxPool {
    #[must_use]
    pub fn pool_id(&self) -> u64 {
        self.pool_id
    }

    #[must_use]
    pub fn slot_count(&self) -> u32 {
        self.surfaces.len() as u32
    }

    /// Producer-side release plumbing calls this when a consumer lease keeps a
    /// slot unavailable after it leaves the ring's live window.
    pub fn set_reuse_blocked(&mut self, slot_id: u32, blocked: bool) -> Result<()> {
        let Some(slot) = self.reuse_blocked.get_mut(slot_id as usize) else {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-synthetic-set-reuse-blocked",
                message: format!("slot {slot_id} outside pool with {} slots", self.surfaces.len()),
            });
        };
        *slot = blocked;
        Ok(())
    }

    fn claim_reusable_slot(&self, candidates: &[SlotReuseCandidate]) -> SlotClaim {
        candidates
            .iter()
            .find(|candidate| !self.reuse_blocked[candidate.slot_id as usize])
            .map(|candidate| SlotClaim::Ready {
                slot_id: candidate.slot_id,
            })
            .unwrap_or(SlotClaim::WouldBlock)
    }
}

#[derive(Debug)]
pub struct SyntheticLinuxFence {
    fence_id: u64,
    timeline: DrmSyncobjTimeline,
}

impl NativeFrameBackend for SyntheticLinuxBackend {
    type CapturedFrame = SyntheticLinuxFrame;
    type SurfacePool = SyntheticLinuxPool;
    type Fence = SyntheticLinuxFence;
    type SurfaceHandle = LinuxSurfaceHandle;
    type SyncHandle = LinuxSyncHandle;

    fn payload_kind(&self) -> PayloadKind {
        PayloadKind::DmaBuf
    }

    fn allocate_surface_pool(&mut self, params: &NativeStreamParams, slot_count: u32) -> Result<Self::SurfacePool> {
        if slot_count == 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-synthetic-allocate-surface-pool",
                message: "slot count must be non-zero".to_string(),
            });
        }
        let (stride, len) = surface_layout(params)?;
        let mut surfaces = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            surfaces.push(self.heap.allocate(len)?);
        }
        let pool_id = self.next_pool_id;
        self.next_pool_id = self.next_pool_id.saturating_add(1);
        Ok(SyntheticLinuxPool {
            pool_id,
            width: params.width,
            height: params.height,
            pixel_format: params.pixel_format,
            modifier: params.modifier,
            stride,
            surfaces,
            reuse_blocked: vec![false; slot_count as usize],
        })
    }

    fn pool_id(&self, pool: &Self::SurfacePool) -> u64 {
        pool.pool_id
    }

    fn claim_reusable_slot(&mut self, pool: &mut Self::SurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
        self.refresh_release_syncs()?;
        for candidate in candidates {
            pool.set_reuse_blocked(
                candidate.slot_id,
                self.lease_book.slot_has_unresolved_leases(pool.pool_id, candidate.slot_id),
            )?;
        }
        Ok(pool.claim_reusable_slot(candidates))
    }

    fn stage_frame(&mut self, _pool: &mut Self::SurfacePool, _slot_id: u32, _frame: &Self::CapturedFrame) -> Result<()> {
        Ok(())
    }

    fn export_surface_handles(&self, pool: &Self::SurfacePool) -> Result<Vec<Self::SurfaceHandle>> {
        pool.surfaces
            .iter()
            .map(|surface| {
                Ok(LinuxSurfaceHandle {
                    handle_kind: FT_NATIVE_HANDLE_DMABUF,
                    width: pool.width,
                    height: pool.height,
                    pixel_format: pool.pixel_format as u32,
                    modifier: pool.modifier,
                    planes: vec![LinuxDmabufPlaneHandle {
                        fd: surface.try_clone_fd()?,
                        offset: 0,
                        stride: pool.stride,
                    }],
                })
            })
            .collect()
    }

    fn create_fence(&mut self) -> Result<Self::Fence> {
        let fence_id = self.next_fence_id;
        self.next_fence_id = self.next_fence_id.saturating_add(1);
        Ok(SyntheticLinuxFence {
            fence_id,
            timeline: self.drm.create_syncobj_timeline()?,
        })
    }

    fn fence_id(&self, fence: &Self::Fence) -> u64 {
        fence.fence_id
    }

    fn signal_fence(&mut self, fence: &mut Self::Fence, value: u64) -> Result<()> {
        fence.timeline.signal(value)
    }

    fn export_sync_handle(&self, fence: &Self::Fence) -> Result<Self::SyncHandle> {
        fence.timeline.export_handle(fence.fence_id)
    }
}

impl LinuxNativeLeaseBackend for SyntheticLinuxBackend {
    fn acquire_linux_lease(&mut self, identity: NativeLeaseIdentity) -> Result<u64> {
        self.lease_book.acquire(identity).map_err(CaptureTransferError::from)
    }

    fn register_linux_release_sync(&mut self, sync: LinuxSyncDescriptor, fd: OwnedFd) -> Result<u64> {
        if sync.sync_kind != FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-synthetic-register-release-sync",
                message: format!("unsupported release sync kind {}", sync.sync_kind),
            });
        }
        let timeline = self.drm.import_syncobj_timeline_fd(fd.as_raw_fd())?;
        let release_sync_id = self.lease_book.register_release_sync().map_err(CaptureTransferError::from)?;
        self.release_syncs.insert(release_sync_id, timeline);
        Ok(release_sync_id)
    }

    fn release_linux_lease(&mut self, lease_id: u64, release: NativeLeaseRelease) -> Result<()> {
        self.lease_book
            .release(lease_id, release)
            .map(|_| ())
            .map_err(CaptureTransferError::from)
    }
}

fn surface_layout(params: &NativeStreamParams) -> Result<(u32, u64)> {
    let bytes_per_pixel = match params.pixel_format {
        PixelFormat::Bgra8Unorm | PixelFormat::Rgba8Unorm => 4_u32,
        PixelFormat::Unknown => {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-synthetic-surface-layout",
                message: "unknown pixel format cannot be allocated as dmabuf".to_string(),
            });
        }
    };
    let stride = params
        .width
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| CaptureTransferError::NativeBackend {
            operation: "linux-synthetic-surface-layout",
            message: format!("width {} overflows stride", params.width),
        })?;
    let len = u64::from(stride)
        .checked_mul(u64::from(params.height))
        .ok_or_else(|| CaptureTransferError::NativeBackend {
            operation: "linux-synthetic-surface-layout",
            message: format!("surface {}x{} overflows allocation length", params.width, params.height),
        })?;
    if len == 0 {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-synthetic-surface-layout",
            message: "surface dimensions must be non-zero".to_string(),
        });
    }
    Ok((stride, len))
}

#[cfg(test)]
mod tests {
    use crate::{
        CaptureTransferError,
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate,
            linux::synthetic::{SyntheticLinuxBackend, surface_layout},
        },
    };

    fn params() -> NativeStreamParams {
        NativeStreamParams {
            width: 64,
            height: 32,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        }
    }

    #[test]
    fn computes_bgra_surface_layout() {
        assert_eq!(surface_layout(&params()).unwrap(), (256, 8192));
    }

    #[test]
    fn rejects_unknown_or_empty_surface_layout() {
        let mut unknown = params();
        unknown.pixel_format = PixelFormat::Unknown;
        assert!(surface_layout(&unknown).is_err());

        let mut empty = params();
        empty.width = 0;
        assert!(surface_layout(&empty).is_err());
    }

    #[test]
    fn opens_real_devices_when_available_and_exports_dmabuf_handles() {
        let mut backend = match SyntheticLinuxBackend::open("/dev/dri/renderD128", "/dev/dma_heap/system") {
            Ok(backend) => backend,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-open" | "linux-drm-open",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected synthetic linux open error: {error}"),
        };

        let pool = backend.allocate_surface_pool(&params(), 2).unwrap();
        assert_eq!(pool.pool_id(), 1);
        assert_eq!(pool.slot_count(), 2);
        let handles = backend.export_surface_handles(&pool).unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0].handle_kind, 2);
        assert_eq!(handles[0].planes.len(), 1);
        assert_eq!(handles[0].planes[0].stride, 256);

        let mut fence = backend.create_fence().unwrap();
        backend.signal_fence(&mut fence, 1).unwrap();
        let sync = backend.export_sync_handle(&fence).unwrap();
        assert_eq!(sync.sync_kind, 2);
        assert_eq!(sync.sync_id, 1);
    }

    #[test]
    fn reuse_claim_respects_blocked_slots() {
        let mut pool = super::SyntheticLinuxPool {
            pool_id: 1,
            width: 1,
            height: 1,
            pixel_format: PixelFormat::Bgra8Unorm,
            modifier: 0,
            stride: 4,
            surfaces: Vec::new(),
            reuse_blocked: vec![true, false],
        };
        let candidates = [
            SlotReuseCandidate {
                slot_id: 0,
                last_cursor: 1,
            },
            SlotReuseCandidate {
                slot_id: 1,
                last_cursor: 2,
            },
        ];
        assert_eq!(pool.claim_reusable_slot(&candidates), SlotClaim::Ready { slot_id: 1 });
        pool.set_reuse_blocked(1, true).unwrap();
        assert_eq!(pool.claim_reusable_slot(&candidates), SlotClaim::WouldBlock);
    }
}
