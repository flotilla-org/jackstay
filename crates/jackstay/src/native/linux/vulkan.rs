//! Narrow Vulkan runtime probe and reference-consumer scaffolding for Linux.
//!
//! The build environment may have a Vulkan loader without development headers.
//! The C shim therefore uses `dlopen`/`vkGetInstanceProcAddr` and only the
//! stable ABI structs needed to ask whether dmabuf and external sync import
//! are viable on this host.

use std::{io, mem::size_of, os::fd::AsRawFd, path::Path};

use crate::{
    error::{CaptureTransferError, Result},
    model::PixelFormat,
    native::linux::{
        LINUX_ATTACH_MAX_PLANES, LinuxAttachGrant, LinuxSurfaceDescriptor, LinuxSyncDescriptor,
        dmabuf::FT_NATIVE_HANDLE_DMABUF,
        drm::{
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, DrmDevice, DrmSyncobjTimeline,
            FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
        },
    },
};

const VK_FORMAT_R8G8B8A8_UNORM: u32 = 37;
const VK_FORMAT_B8G8R8A8_UNORM: u32 = 44;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxVulkanProbe {
    struct_size: u32,
    loader_present: u32,
    can_enumerate_instance_extensions: u32,
    can_create_instance: u32,
    physical_device_count: u32,
    instance_extension_count: u32,
    has_get_physical_device_properties2: u32,
    has_external_memory_capabilities: u32,
    has_external_semaphore_capabilities: u32,
    has_external_fence_capabilities: u32,
    has_external_memory_dma_buf: u32,
    has_external_memory_fd: u32,
    has_image_drm_format_modifier: u32,
    has_get_memory_requirements2: u32,
    has_bind_memory2: u32,
    has_queue_family_foreign: u32,
    has_external_semaphore_fd: u32,
    has_external_fence_fd: u32,
    has_timeline_semaphore: u32,
    can_create_reference_device: u32,
    has_image_import_device_functions: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct PortholeNativeLinuxVulkanImportPlane {
    offset: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxVulkanImportImage {
    struct_size: u32,
    fd: i32,
    width: u32,
    height: u32,
    vk_format: u32,
    modifier: u64,
    plane_count: u32,
    planes: [PortholeNativeLinuxVulkanImportPlane; LINUX_ATTACH_MAX_PLANES],
}

#[repr(C)]
#[derive(Debug)]
struct PortholeNativeLinuxVulkanModifierQuery {
    struct_size: u32,
    vk_format: u32,
    modifier_capacity: u32,
    modifier_count: u32,
    modifiers: *mut u64,
}

impl Default for PortholeNativeLinuxVulkanProbe {
    fn default() -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            loader_present: 0,
            can_enumerate_instance_extensions: 0,
            can_create_instance: 0,
            physical_device_count: 0,
            instance_extension_count: 0,
            has_get_physical_device_properties2: 0,
            has_external_memory_capabilities: 0,
            has_external_semaphore_capabilities: 0,
            has_external_fence_capabilities: 0,
            has_external_memory_dma_buf: 0,
            has_external_memory_fd: 0,
            has_image_drm_format_modifier: 0,
            has_get_memory_requirements2: 0,
            has_bind_memory2: 0,
            has_queue_family_foreign: 0,
            has_external_semaphore_fd: 0,
            has_external_fence_fd: 0,
            has_timeline_semaphore: 0,
            can_create_reference_device: 0,
            has_image_import_device_functions: 0,
        }
    }
}

mod ffi {
    use super::{PortholeNativeLinuxVulkanImportImage, PortholeNativeLinuxVulkanModifierQuery, PortholeNativeLinuxVulkanProbe};

    unsafe extern "C" {
        pub fn porthole_native_linux_vulkan_probe(out: *mut PortholeNativeLinuxVulkanProbe) -> i32;
        pub fn porthole_native_linux_vulkan_import_dmabuf_image(image: *const PortholeNativeLinuxVulkanImportImage) -> i32;
        pub fn porthole_native_linux_vulkan_query_format_modifiers(query: *mut PortholeNativeLinuxVulkanModifierQuery) -> i32;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VulkanRuntimeProbe {
    pub loader_present: bool,
    pub can_enumerate_instance_extensions: bool,
    pub can_create_instance: bool,
    pub physical_device_count: u32,
    pub instance_extension_count: u32,
    pub has_get_physical_device_properties2: bool,
    pub has_external_memory_capabilities: bool,
    pub has_external_semaphore_capabilities: bool,
    pub has_external_fence_capabilities: bool,
    pub has_external_memory_dma_buf: bool,
    pub has_external_memory_fd: bool,
    pub has_image_drm_format_modifier: bool,
    pub has_get_memory_requirements2: bool,
    pub has_bind_memory2: bool,
    pub has_queue_family_foreign: bool,
    pub has_external_semaphore_fd: bool,
    pub has_external_fence_fd: bool,
    pub has_timeline_semaphore: bool,
    pub can_create_reference_device: bool,
    pub has_image_import_device_functions: bool,
}

impl VulkanRuntimeProbe {
    pub fn probe() -> Result<Self> {
        let mut raw = PortholeNativeLinuxVulkanProbe::default();
        let errno = unsafe { ffi::porthole_native_linux_vulkan_probe(&mut raw) };
        if errno != 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-vulkan-probe",
                message: io::Error::from_raw_os_error(errno).to_string(),
            });
        }
        Ok(Self {
            loader_present: raw.loader_present != 0,
            can_enumerate_instance_extensions: raw.can_enumerate_instance_extensions != 0,
            can_create_instance: raw.can_create_instance != 0,
            physical_device_count: raw.physical_device_count,
            instance_extension_count: raw.instance_extension_count,
            has_get_physical_device_properties2: raw.has_get_physical_device_properties2 != 0,
            has_external_memory_capabilities: raw.has_external_memory_capabilities != 0,
            has_external_semaphore_capabilities: raw.has_external_semaphore_capabilities != 0,
            has_external_fence_capabilities: raw.has_external_fence_capabilities != 0,
            has_external_memory_dma_buf: raw.has_external_memory_dma_buf != 0,
            has_external_memory_fd: raw.has_external_memory_fd != 0,
            has_image_drm_format_modifier: raw.has_image_drm_format_modifier != 0,
            has_get_memory_requirements2: raw.has_get_memory_requirements2 != 0,
            has_bind_memory2: raw.has_bind_memory2 != 0,
            has_queue_family_foreign: raw.has_queue_family_foreign != 0,
            has_external_semaphore_fd: raw.has_external_semaphore_fd != 0,
            has_external_fence_fd: raw.has_external_fence_fd != 0,
            has_timeline_semaphore: raw.has_timeline_semaphore != 0,
            can_create_reference_device: raw.can_create_reference_device != 0,
            has_image_import_device_functions: raw.has_image_import_device_functions != 0,
        })
    }

    #[must_use]
    pub fn supports_dmabuf_import(&self) -> bool {
        self.loader_present
            && self.can_create_instance
            && self.physical_device_count > 0
            && self.has_external_memory_capabilities
            && self.has_external_memory_dma_buf
            && self.has_external_memory_fd
            && self.has_image_drm_format_modifier
            && self.has_get_memory_requirements2
            && self.has_bind_memory2
            && self.has_queue_family_foreign
            && self.can_create_reference_device
            && self.has_image_import_device_functions
    }

    #[must_use]
    pub fn supports_external_sync_for(&self, sync_kind: u32) -> bool {
        match sync_kind {
            FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE => {
                self.loader_present
                    && self.can_create_instance
                    && self.physical_device_count > 0
                    && self.has_external_semaphore_capabilities
                    && self.has_external_semaphore_fd
                    && self.has_timeline_semaphore
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn supports_reference_consumer_primitives(&self) -> bool {
        self.supports_dmabuf_import() && self.supports_external_sync_for(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VulkanReferencePlane {
    pub fd_index: u32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VulkanReferenceSurface {
    pub pool_id: u64,
    pub slot_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub planes: Vec<VulkanReferencePlane>,
}

#[derive(Debug)]
pub struct VulkanReferenceConsumer {
    producer_sync_id: u64,
    producer_timeline: DrmSyncobjTimeline,
    surfaces: Vec<VulkanReferenceSurface>,
}

pub fn supported_format_modifiers(pixel_format: PixelFormat, capacity: usize) -> Result<Vec<u64>> {
    let vk_format = match pixel_format {
        PixelFormat::Rgba8Unorm => VK_FORMAT_R8G8B8A8_UNORM,
        PixelFormat::Bgra8Unorm => VK_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Unknown => {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-vulkan-query-format-modifiers",
                message: "unknown pixel format has no Vulkan modifier query".to_string(),
            });
        }
    };
    if capacity == 0 {
        return Ok(Vec::new());
    }
    let mut modifiers = vec![0u64; capacity];
    let mut query = PortholeNativeLinuxVulkanModifierQuery {
        struct_size: size_of::<PortholeNativeLinuxVulkanModifierQuery>() as u32,
        vk_format,
        modifier_capacity: capacity as u32,
        modifier_count: 0,
        modifiers: modifiers.as_mut_ptr(),
    };
    let errno = unsafe { ffi::porthole_native_linux_vulkan_query_format_modifiers(&mut query) };
    if errno != 0 {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-query-format-modifiers",
            message: io::Error::from_raw_os_error(errno).to_string(),
        });
    }
    modifiers.truncate(query.modifier_count as usize);
    Ok(modifiers)
}

impl VulkanReferenceConsumer {
    pub fn import_grant(drm_render_path: impl AsRef<Path>, grant: &LinuxAttachGrant, fds: &[impl AsRawFd]) -> Result<Self> {
        Self::import_grant_with_probe(VulkanRuntimeProbe::probe()?, drm_render_path, grant, fds)
    }

    pub fn import_grant_with_probe(
        probe: VulkanRuntimeProbe,
        drm_render_path: impl AsRef<Path>,
        grant: &LinuxAttachGrant,
        fds: &[impl AsRawFd],
    ) -> Result<Self> {
        if !probe.supports_reference_consumer_primitives() {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-vulkan-reference-consumer",
                message: "Vulkan runtime does not support dmabuf image import plus DRM syncobj timeline waits".to_string(),
            });
        }
        grant.validate_fd_indices(fds.len())?;
        validate_producer_sync(&grant.producer_sync)?;
        let drm = DrmDevice::open(drm_render_path)?;
        if !drm.supports_syncobj_timeline()? {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-vulkan-reference-consumer",
                message: "DRM render node does not support syncobj timelines".to_string(),
            });
        }
        let producer_timeline = drm.import_syncobj_timeline_fd(fds[grant.producer_sync.fd_index as usize].as_raw_fd())?;
        let surfaces = validate_reference_surfaces(grant)?;
        Ok(Self {
            producer_sync_id: grant.producer_sync.sync_id,
            producer_timeline,
            surfaces,
        })
    }

    #[must_use]
    pub fn producer_sync_id(&self) -> u64 {
        self.producer_sync_id
    }

    #[must_use]
    pub fn surfaces(&self) -> &[VulkanReferenceSurface] {
        &self.surfaces
    }

    pub fn wait_producer_fence(&self, producer_sync_id: u64, value: u64, timeout_ns: u64) -> Result<()> {
        if producer_sync_id != self.producer_sync_id {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-vulkan-wait-producer-fence",
                message: format!("unknown producer sync id {producer_sync_id}"),
            });
        }
        self.producer_timeline.wait(
            value,
            timeout_ns,
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
        )
    }

    pub fn try_import_surface_image(&self, surface: &VulkanReferenceSurface, fds: &[impl AsRawFd]) -> Result<()> {
        import_surface_image(surface, fds)
    }
}

pub fn import_surface_image(surface: &VulkanReferenceSurface, fds: &[impl AsRawFd]) -> Result<()> {
    if surface.planes.len() != 1 {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-import-dmabuf-image",
            message: format!("only single-plane dmabuf image import is implemented, got {}", surface.planes.len()),
        });
    }
    let vk_format = vk_format_for_pixel_format(surface.pixel_format)?;
    let plane = &surface.planes[0];
    if (plane.fd_index as usize) >= fds.len() {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-import-dmabuf-image",
            message: format!("plane fd index {} outside fd table of {}", plane.fd_index, fds.len()),
        });
    }
    let mut raw = PortholeNativeLinuxVulkanImportImage {
        struct_size: size_of::<PortholeNativeLinuxVulkanImportImage>() as u32,
        fd: fds[plane.fd_index as usize].as_raw_fd(),
        width: surface.width,
        height: surface.height,
        vk_format,
        modifier: surface.modifier,
        plane_count: 1,
        planes: [PortholeNativeLinuxVulkanImportPlane::default(); LINUX_ATTACH_MAX_PLANES],
    };
    raw.planes[0] = PortholeNativeLinuxVulkanImportPlane {
        offset: plane.offset,
        stride: plane.stride,
    };
    let errno = unsafe { ffi::porthole_native_linux_vulkan_import_dmabuf_image(&raw) };
    if errno != 0 {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-import-dmabuf-image",
            message: io::Error::from_raw_os_error(errno).to_string(),
        });
    }
    Ok(())
}

fn vk_format_for_pixel_format(pixel_format: u32) -> Result<u32> {
    match pixel_format {
        value if value == PixelFormat::Rgba8Unorm as u32 => Ok(VK_FORMAT_R8G8B8A8_UNORM),
        value if value == PixelFormat::Bgra8Unorm as u32 => Ok(VK_FORMAT_B8G8R8A8_UNORM),
        _ => Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-import-dmabuf-image",
            message: format!("unsupported pixel format {pixel_format}"),
        }),
    }
}

fn validate_producer_sync(sync: &LinuxSyncDescriptor) -> Result<()> {
    if sync.sync_kind != FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: format!("unsupported producer sync kind {}", sync.sync_kind),
        });
    }
    Ok(())
}

fn validate_reference_surfaces(grant: &LinuxAttachGrant) -> Result<Vec<VulkanReferenceSurface>> {
    let mut surfaces = Vec::new();
    for pool in &grant.pools {
        for (slot_id, surface) in pool.surfaces.iter().enumerate() {
            validate_dmabuf_surface(surface)?;
            surfaces.push(VulkanReferenceSurface {
                pool_id: pool.pool_id,
                slot_id: slot_id as u32,
                width: surface.width,
                height: surface.height,
                pixel_format: surface.pixel_format,
                modifier: surface.modifier,
                planes: surface
                    .planes
                    .iter()
                    .map(|plane| VulkanReferencePlane {
                        fd_index: plane.fd_index,
                        offset: plane.offset,
                        stride: plane.stride,
                    })
                    .collect(),
            });
        }
    }
    if surfaces.is_empty() {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: "grant contains no dmabuf surfaces".to_string(),
        });
    }
    Ok(surfaces)
}

fn validate_dmabuf_surface(surface: &LinuxSurfaceDescriptor) -> Result<()> {
    if surface.handle_kind != FT_NATIVE_HANDLE_DMABUF {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: format!("unsupported surface handle kind {}", surface.handle_kind),
        });
    }
    if surface.width == 0 || surface.height == 0 {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: format!("invalid surface dimensions {}x{}", surface.width, surface.height),
        });
    }
    if surface.planes.is_empty() || surface.planes.len() > LINUX_ATTACH_MAX_PLANES {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: format!("dmabuf plane count {} outside 1..={LINUX_ATTACH_MAX_PLANES}", surface.planes.len()),
        });
    }
    if surface.planes.iter().any(|plane| plane.stride == 0) {
        return Err(CaptureTransferError::NativeBackend {
            operation: "linux-vulkan-reference-consumer",
            message: "dmabuf plane stride must be non-zero".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Write, os::fd::OwnedFd};

    use super::{
        VulkanReferenceConsumer, VulkanReferencePlane, VulkanReferenceSurface, VulkanRuntimeProbe, import_surface_image,
        supported_format_modifiers, validate_reference_surfaces,
    };
    use crate::{
        CaptureTransferError,
        model::PixelFormat,
        native::linux::{
            LinuxAttachGrant, LinuxPlaneDescriptor, LinuxPoolDescriptor, LinuxSurfaceDescriptor, LinuxSyncDescriptor,
            dmabuf::{DmaHeap, FT_NATIVE_HANDLE_DMABUF},
            drm::{DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED, DrmDevice, FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE},
        },
    };

    #[test]
    fn vulkan_runtime_probe_reports_consistent_capabilities() {
        let probe = VulkanRuntimeProbe::probe().unwrap();
        if !probe.loader_present {
            assert!(!probe.supports_dmabuf_import());
            assert!(!probe.supports_reference_consumer_primitives());
            return;
        }

        assert!(probe.can_enumerate_instance_extensions);
        if probe.can_create_instance {
            assert!(probe.instance_extension_count > 0);
        } else {
            assert_eq!(probe.physical_device_count, 0);
        }

        if probe.supports_reference_consumer_primitives() {
            assert!(probe.supports_dmabuf_import());
            assert!(probe.supports_external_sync_for(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE));
        }
        assert!(!probe.supports_external_sync_for(0));
    }

    #[test]
    fn dmabuf_import_requires_modifier_and_memory_binding_extensions() {
        let mut probe = supported_probe();
        assert!(probe.supports_dmabuf_import());

        probe.has_image_drm_format_modifier = false;
        assert!(!probe.supports_dmabuf_import());

        let mut probe = supported_probe();
        probe.has_get_memory_requirements2 = false;
        assert!(!probe.supports_dmabuf_import());

        let mut probe = supported_probe();
        probe.has_bind_memory2 = false;
        assert!(!probe.supports_dmabuf_import());

        let mut probe = supported_probe();
        probe.has_queue_family_foreign = false;
        assert!(!probe.supports_dmabuf_import());

        let mut probe = supported_probe();
        probe.can_create_reference_device = false;
        assert!(!probe.supports_dmabuf_import());

        let mut probe = supported_probe();
        probe.has_image_import_device_functions = false;
        assert!(!probe.supports_dmabuf_import());
    }

    #[test]
    fn format_modifier_query_returns_bounded_unique_modifiers_when_supported() {
        let modifiers = match supported_format_modifiers(PixelFormat::Bgra8Unorm, 16) {
            Ok(modifiers) => modifiers,
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("Function not implemented") || message.contains("No such device") || message.contains("I/O error"),
                    "unexpected modifier query error: {message}"
                );
                return;
            }
        };
        assert!(modifiers.len() <= 16);
        for (index, modifier) in modifiers.iter().enumerate() {
            assert_eq!(modifiers.iter().position(|candidate| candidate == modifier), Some(index));
        }
    }

    #[test]
    fn validates_reference_consumer_surface_descriptors() {
        let grant = test_grant(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, FT_NATIVE_HANDLE_DMABUF, 1);
        let surfaces = validate_reference_surfaces(&grant).unwrap();
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].pool_id, 77);
        assert_eq!(surfaces[0].slot_id, 0);
        assert_eq!(surfaces[0].planes.len(), 1);
        assert_eq!(surfaces[0].planes[0].fd_index, 1);
        assert_eq!(surfaces[0].planes[0].offset, 0);
        assert_eq!(surfaces[0].planes[0].stride, 1);
    }

    #[test]
    fn rejects_reference_consumer_grant_without_dmabuf_surface() {
        let wrong_kind = test_grant(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, 999, 1);
        assert!(validate_reference_surfaces(&wrong_kind).is_err());

        let zero_stride = test_grant(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, FT_NATIVE_HANDLE_DMABUF, 0);
        assert!(validate_reference_surfaces(&zero_stride).is_err());
    }

    #[test]
    fn reference_consumer_rejects_unsupported_runtime_before_opening_drm() {
        let grant = test_grant(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, FT_NATIVE_HANDLE_DMABUF, 1);
        let error = VulkanReferenceConsumer::import_grant_with_probe(unsupported_probe(), "/definitely/not/a/render-node", &grant, &[0])
            .unwrap_err();
        assert!(error.to_string().contains("does not support dmabuf image import"));
    }

    #[test]
    fn import_surface_image_rejects_unsupported_descriptor_shape() {
        let surface = VulkanReferenceSurface {
            pool_id: 77,
            slot_id: 0,
            width: 64,
            height: 32,
            pixel_format: PixelFormat::Unknown as u32,
            modifier: 0,
            planes: vec![VulkanReferencePlane {
                fd_index: 0,
                offset: 0,
                stride: 256,
            }],
        };
        assert!(import_surface_image(&surface, &[0]).is_err());

        let mut surface = surface;
        surface.pixel_format = PixelFormat::Bgra8Unorm as u32;
        surface.planes.push(VulkanReferencePlane {
            fd_index: 0,
            offset: 0,
            stride: 256,
        });
        assert!(import_surface_image(&surface, &[0]).is_err());
    }

    #[test]
    fn reference_consumer_imports_and_waits_real_syncobj_timeline_when_available() {
        let probe = VulkanRuntimeProbe::probe().unwrap();
        if !probe.supports_reference_consumer_primitives() {
            return;
        }
        let Ok(device) = DrmDevice::open("/dev/dri/renderD128") else {
            return;
        };
        if !device.supports_syncobj_timeline().unwrap_or(false) {
            return;
        }

        let producer_timeline = device.create_syncobj_timeline().unwrap();
        producer_timeline.signal(5).unwrap();
        assert_eq!(producer_timeline.query(DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED).unwrap(), 5);

        let fds = vec![tempfile_fd(), tempfile_fd(), producer_timeline.export_fd().unwrap()];
        let grant = test_grant(FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, FT_NATIVE_HANDLE_DMABUF, 256);
        let consumer = VulkanReferenceConsumer::import_grant_with_probe(probe, "/dev/dri/renderD128", &grant, &fds).unwrap();

        assert_eq!(consumer.producer_sync_id(), 99);
        assert_eq!(consumer.surfaces().len(), 1);
        consumer.wait_producer_fence(99, 5, 0).unwrap();
        assert!(consumer.wait_producer_fence(100, 5, 0).is_err());
    }

    #[test]
    fn imports_real_single_plane_dmabuf_image_when_available() {
        let probe = VulkanRuntimeProbe::probe().unwrap();
        if !probe.supports_dmabuf_import() {
            return;
        }
        let heap = match DmaHeap::open("/dev/dma_heap/system") {
            Ok(heap) => heap,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-open",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected dma-heap open error: {error}"),
        };
        let dmabuf = heap.allocate(4096).unwrap();
        let fd = dmabuf.try_clone_fd().unwrap();
        let surface = VulkanReferenceSurface {
            pool_id: 77,
            slot_id: 0,
            width: 16,
            height: 16,
            pixel_format: PixelFormat::Bgra8Unorm as u32,
            modifier: 0,
            planes: vec![VulkanReferencePlane {
                fd_index: 0,
                offset: 0,
                stride: 64,
            }],
        };
        import_surface_image(&surface, &[fd]).unwrap();
    }

    fn test_grant(sync_kind: u32, handle_kind: u32, stride: u32) -> LinuxAttachGrant {
        LinuxAttachGrant {
            consumer_id: 1,
            consumer_slot: 0,
            ring_fd_index: 0,
            ring_map_len: 4096,
            pools: vec![LinuxPoolDescriptor {
                pool_id: 77,
                surfaces: vec![LinuxSurfaceDescriptor {
                    handle_kind,
                    width: 64,
                    height: 32,
                    pixel_format: 1,
                    modifier: 0,
                    planes: vec![LinuxPlaneDescriptor {
                        fd_index: 1,
                        offset: 0,
                        stride,
                    }],
                }],
            }],
            producer_sync: LinuxSyncDescriptor {
                sync_kind,
                sync_id: 99,
                fd_index: 2,
            },
        }
    }

    fn unsupported_probe() -> VulkanRuntimeProbe {
        VulkanRuntimeProbe {
            loader_present: false,
            can_enumerate_instance_extensions: false,
            can_create_instance: false,
            physical_device_count: 0,
            instance_extension_count: 0,
            has_get_physical_device_properties2: false,
            has_external_memory_capabilities: false,
            has_external_semaphore_capabilities: false,
            has_external_fence_capabilities: false,
            has_external_memory_dma_buf: false,
            has_external_memory_fd: false,
            has_image_drm_format_modifier: false,
            has_get_memory_requirements2: false,
            has_bind_memory2: false,
            has_queue_family_foreign: false,
            has_external_semaphore_fd: false,
            has_external_fence_fd: false,
            has_timeline_semaphore: false,
            can_create_reference_device: false,
            has_image_import_device_functions: false,
        }
    }

    fn supported_probe() -> VulkanRuntimeProbe {
        VulkanRuntimeProbe {
            loader_present: true,
            can_enumerate_instance_extensions: true,
            can_create_instance: true,
            physical_device_count: 1,
            instance_extension_count: 4,
            has_get_physical_device_properties2: true,
            has_external_memory_capabilities: true,
            has_external_semaphore_capabilities: true,
            has_external_fence_capabilities: true,
            has_external_memory_dma_buf: true,
            has_external_memory_fd: true,
            has_image_drm_format_modifier: true,
            has_get_memory_requirements2: true,
            has_bind_memory2: true,
            has_queue_family_foreign: true,
            has_external_semaphore_fd: true,
            has_external_fence_fd: true,
            has_timeline_semaphore: true,
            can_create_reference_device: true,
            has_image_import_device_functions: true,
        }
    }

    fn tempfile_fd() -> OwnedFd {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"fd").unwrap();
        OwnedFd::from(file)
    }
}
