//! Retained Vulkan resources for rendering and reading back without a window.
//!
//! This module deliberately has no dependency on winit, raw window handles,
//! `ash-window`, surfaces, or swapchains. The process-level context owns the
//! Vulkan instance/device/queue. Sessions receive independent cached targets,
//! and each target retains its own image, readback allocation, command pool,
//! command buffer, and fence.

use ash::Entry;
use ash::vk;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

const DEFAULT_TARGET_CACHE_CAPACITY: usize = 8;
const FENCE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const OFFSCREEN_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Facts recorded while selecting the no-window Vulkan device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanDeviceFacts {
    /// Human-readable physical-device name reported by Vulkan.
    pub device_name: String,
    /// Vulkan vendor identifier.
    pub vendor_id: u32,
    /// Vulkan device identifier.
    pub device_id: u32,
    /// Driver version reported by Vulkan.
    pub driver_version: u32,
    /// Vulkan API version supported by the physical device.
    pub api_version: u32,
    /// Selected graphics queue-family index.
    pub queue_family_index: u32,
    /// Queue capabilities for the selected family.
    pub queue_flags: vk::QueueFlags,
    /// Maximum two-dimensional image size.
    pub max_image_dimension_2d: u32,
    /// Instance extensions enabled by the no-window runtime. Always empty.
    pub enabled_instance_extensions: Vec<String>,
    /// Device extensions enabled by the no-window runtime. Always empty.
    pub enabled_device_extensions: Vec<String>,
}

/// Stable classification for an unavailable offscreen GPU runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VulkanOffscreenUnavailableKind {
    /// The Vulkan loader could not be loaded.
    Loader,
    /// A Vulkan instance could not be created or queried.
    Instance,
    /// No physical device met the no-window transfer requirements.
    Device,
    /// The logical device could not be created.
    LogicalDevice,
}

/// Actionable cached explanation for GPU unavailability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenUnavailable {
    /// Stable unavailability category.
    pub kind: VulkanOffscreenUnavailableKind,
    /// Human-readable diagnostic suitable for logs or capability UI.
    pub message: String,
}

/// Result of probing the true no-window Vulkan path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VulkanOffscreenAvailability {
    /// A compatible device was created successfully.
    Available(VulkanDeviceFacts),
    /// Vulkan is unavailable with a typed diagnostic.
    Unavailable(VulkanOffscreenUnavailable),
}

/// Typed failures from the no-window Vulkan runtime.
#[derive(Debug)]
pub enum VulkanOffscreenError {
    /// Dynamic Vulkan loader discovery failed.
    LoaderUnavailable { detail: String },
    /// Vulkan instance construction failed.
    InstanceCreation { source: vk::Result },
    /// Physical-device enumeration failed.
    DeviceEnumeration { source: vk::Result },
    /// No physical device exposes the required queue and RGBA transfer format.
    NoCompatibleDevice { detail: String },
    /// Logical-device construction failed.
    DeviceCreation { source: vk::Result },
    /// A requested image extent was zero, overflowed, or exceeded device limits.
    InvalidExtent {
        width: u32,
        height: u32,
        detail: String,
    },
    /// A bounded cache with zero capacity was requested.
    InvalidCacheCapacity,
    /// A Vulkan image, memory, buffer, command, or synchronization allocation failed.
    Allocation {
        resource: &'static str,
        source: vk::Result,
    },
    /// A Vulkan command or host operation failed.
    Operation {
        operation: &'static str,
        source: vk::Result,
    },
    /// A submitted operation did not complete before its deadline.
    Timeout { timeout: Duration },
    /// Work was cancelled. Submitted work remains safely owned until completion.
    Cancelled,
    /// Vulkan reported device loss. The context must no longer be used.
    DeviceLost { operation: &'static str },
}

impl fmt::Display for VulkanOffscreenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoaderUnavailable { detail } => write!(
                formatter,
                "Vulkan loader unavailable; install a Vulkan runtime/driver: {detail}"
            ),
            Self::InstanceCreation { source } => {
                write!(
                    formatter,
                    "failed to create no-window Vulkan instance: {source:?}"
                )
            }
            Self::DeviceEnumeration { source } => write!(
                formatter,
                "failed to enumerate Vulkan physical devices: {source:?}"
            ),
            Self::NoCompatibleDevice { detail } => {
                write!(formatter, "no compatible no-window Vulkan device: {detail}")
            }
            Self::DeviceCreation { source } => write!(
                formatter,
                "failed to create no-window Vulkan logical device: {source:?}"
            ),
            Self::InvalidExtent {
                width,
                height,
                detail,
            } => write!(
                formatter,
                "invalid Vulkan offscreen extent {width}x{height}: {detail}"
            ),
            Self::InvalidCacheCapacity => {
                formatter.write_str("Vulkan offscreen target cache capacity must be non-zero")
            }
            Self::Allocation { resource, source } => {
                write!(
                    formatter,
                    "failed to allocate Vulkan {resource}: {source:?}"
                )
            }
            Self::Operation { operation, source } => {
                write!(formatter, "Vulkan {operation} failed: {source:?}")
            }
            Self::Timeout { timeout } => write!(
                formatter,
                "Vulkan offscreen work exceeded the {:?} completion deadline",
                timeout
            ),
            Self::Cancelled => formatter.write_str("Vulkan offscreen work was cancelled"),
            Self::DeviceLost { operation } => write!(
                formatter,
                "Vulkan device was lost during {operation}; retire GPU presentations"
            ),
        }
    }
}

impl std::error::Error for VulkanOffscreenError {}

impl VulkanOffscreenUnavailable {
    fn from_error(error: &VulkanOffscreenError) -> Self {
        let kind = match error {
            VulkanOffscreenError::LoaderUnavailable { .. } => {
                VulkanOffscreenUnavailableKind::Loader
            }
            VulkanOffscreenError::InstanceCreation { .. }
            | VulkanOffscreenError::DeviceEnumeration { .. } => {
                VulkanOffscreenUnavailableKind::Instance
            }
            VulkanOffscreenError::NoCompatibleDevice { .. } => {
                VulkanOffscreenUnavailableKind::Device
            }
            VulkanOffscreenError::DeviceCreation { .. } => {
                VulkanOffscreenUnavailableKind::LogicalDevice
            }
            VulkanOffscreenError::InvalidExtent { .. }
            | VulkanOffscreenError::InvalidCacheCapacity
            | VulkanOffscreenError::Allocation { .. }
            | VulkanOffscreenError::Operation { .. }
            | VulkanOffscreenError::Timeout { .. }
            | VulkanOffscreenError::Cancelled
            | VulkanOffscreenError::DeviceLost { .. } => VulkanOffscreenUnavailableKind::Device,
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

/// Cooperative cancellation token for a submitted offscreen operation.
#[derive(Clone, Debug, Default)]
pub struct VulkanOffscreenCancellation {
    cancelled: Arc<AtomicBool>,
}

impl VulkanOffscreenCancellation {
    /// Request cancellation. GPU ownership remains valid until its fence completes.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Current bounded target-cache counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenCacheStats {
    /// Maximum number of retained per-session/per-size targets.
    pub capacity: usize,
    /// Number of currently retained cache entries.
    pub retained_targets: usize,
    /// Successful target allocations.
    pub allocations: u64,
    /// Cache hits returning an existing allocation.
    pub reuses: u64,
    /// Targets removed by bounded LRU eviction.
    pub evictions: u64,
    /// Target allocations whose Vulkan resources have been destroyed.
    pub destructions: u64,
}

/// CPU-visible bytes read back from a no-window Vulkan image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenReadback {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 pixels.
    pub rgba: Vec<u8>,
    /// Distinct CPU/GPU/readback stage timings for this operation.
    pub timings: VulkanOffscreenStageTimings,
}

/// One bounded top-left image region requested for tight GPU readback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenRegion {
    /// Left pixel coordinate.
    pub x: u32,
    /// Top pixel coordinate.
    pub y: u32,
    /// Region width.
    pub width: u32,
    /// Region height.
    pub height: u32,
}

/// Tight bytes for one requested no-window image region.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenRegionReadback {
    /// Source region copied from the GPU image.
    pub region: VulkanOffscreenRegion,
    /// Tight top-left RGBA8 rows for this region only.
    pub rgba: Vec<u8>,
}

/// Regional-only readback; no full-frame GPU-to-host copy is implied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanOffscreenRegionalReadback {
    /// Native source image width.
    pub frame_width: u32,
    /// Native source image height.
    pub frame_height: u32,
    /// Ordered tight region payloads.
    pub regions: Vec<VulkanOffscreenRegionReadback>,
    /// Distinct CPU/GPU/readback stage timings for this operation.
    pub timings: VulkanOffscreenStageTimings,
}

/// Timings measured inside one retained offscreen submission/readback operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VulkanOffscreenStageTimings {
    /// CPU command-buffer reset, barriers, caller recording, and finalization.
    pub command_record_us: u64,
    /// CPU queue-lock plus `vkQueueSubmit` duration.
    pub queue_submit_us: u64,
    /// Measured completion-fence wait after submission.
    pub gpu_completion_wait_us: u64,
    /// Host memory map/invalidate/copy/unmap duration.
    pub readback_map_copy_us: u64,
}

/// Process-shareable retained Vulkan instance/device/queue and target cache.
#[derive(Clone)]
pub struct VulkanOffscreenContext {
    inner: Arc<ContextInner>,
}

impl fmt::Debug for VulkanOffscreenContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanOffscreenContext")
            .field("device", &self.inner.device.facts)
            .field("cache", &self.cache_stats())
            .field("device_lost", &self.is_device_lost())
            .finish_non_exhaustive()
    }
}

/// Per-terminal/session ownership boundary for independent image state.
pub struct VulkanOffscreenSession {
    lease: Arc<SessionLease>,
}

impl Clone for VulkanOffscreenSession {
    fn clone(&self) -> Self {
        Self {
            lease: Arc::clone(&self.lease),
        }
    }
}

impl fmt::Debug for VulkanOffscreenSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanOffscreenSession")
            .field("id", &self.lease.id)
            .finish_non_exhaustive()
    }
}

/// A retained per-session/per-size Vulkan image and readback path.
#[derive(Clone)]
pub struct VulkanOffscreenTarget {
    resources: Arc<TargetResources>,
}

/// Crate-internal retained-device seam for constructing shader/pipeline state.
///
/// Raw Vulkan handles never cross the renderer crate's public API. A sibling
/// slug module can use this borrowed view to create device-scoped compute
/// resources that are retained alongside `VulkanOffscreenContext`.
pub(crate) struct VulkanOffscreenDeviceAccess<'a> {
    instance: &'a ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: &'a ash::Device,
    queue_family_index: u32,
}

/// Crate-internal recording view supplied while a retained target is locked.
///
/// The caller records only pass-local commands. Target reset, image layout
/// transitions, queue external synchronization, fence ownership, timeout/
/// cancellation handling, and readback remain owned by this module.
pub(crate) struct VulkanOffscreenRecording<'a> {
    device: &'a ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    image_view: vk::ImageView,
    extent: vk::Extent2D,
}

impl fmt::Debug for VulkanOffscreenTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VulkanOffscreenTarget")
            .field("allocation_id", &self.resources.allocation_id)
            .field("width", &self.resources.key.width)
            .field("height", &self.resources.key.height)
            .field("session_id", &self.resources.key.session_id)
            .finish_non_exhaustive()
    }
}

impl VulkanOffscreenDeviceAccess<'_> {
    /// Retained Vulkan instance used to query/create device-scoped resources.
    pub(crate) fn instance(&self) -> &ash::Instance {
        self.instance
    }

    /// Selected physical-device handle.
    pub(crate) fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    /// Retained logical device used to create compute pipeline state.
    pub(crate) fn device(&self) -> &ash::Device {
        self.device
    }

    /// Graphics/compute queue family used by retained target command pools.
    pub(crate) fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }

    /// Storage/transfer image format guaranteed by the context probe.
    pub(crate) fn target_format(&self) -> vk::Format {
        OFFSCREEN_FORMAT
    }
}

impl VulkanOffscreenRecording<'_> {
    /// Logical device on which the target command buffer is recording.
    pub(crate) fn device(&self) -> &ash::Device {
        self.device
    }

    /// Exclusively-held command buffer inside an active pass.
    pub(crate) fn command_buffer(&self) -> vk::CommandBuffer {
        self.command_buffer
    }

    /// Retained target image in the pass layout documented by the caller seam.
    pub(crate) fn image(&self) -> vk::Image {
        self.image
    }

    /// Retained storage-image view for compute descriptor binding.
    pub(crate) fn image_view(&self) -> vk::ImageView {
        self.image_view
    }

    /// Native target extent in pixels.
    pub(crate) fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// Storage/transfer image format.
    pub(crate) fn format(&self) -> vk::Format {
        OFFSCREEN_FORMAT
    }
}

struct ContextInner {
    // Drop the cache (and therefore cached targets) before releasing the
    // process-level device reference.
    cache: Mutex<ContextCache>,
    device: Arc<DeviceShared>,
    next_session_id: AtomicU64,
    next_allocation_id: AtomicU64,
}

struct DeviceShared {
    _entry: Entry,
    instance: ash::Instance,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    queue_family_index: u32,
    queue: vk::Queue,
    queue_submit: Mutex<()>,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    facts: VulkanDeviceFacts,
    device_lost: AtomicBool,
    allocations: AtomicU64,
    destructions: AtomicU64,
}

impl fmt::Debug for DeviceShared {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceShared")
            .field("physical_device", &self.physical_device)
            .field("facts", &self.facts)
            .field("device_lost", &self.device_lost.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TargetKey {
    session_id: u64,
    width: u32,
    height: u32,
}

struct CacheEntry {
    target: Arc<TargetResources>,
    last_used: u64,
}

struct ContextCache {
    capacity: usize,
    tick: u64,
    entries: HashMap<TargetKey, CacheEntry>,
    reuses: u64,
    evictions: u64,
}

struct SessionLease {
    id: u64,
    context: Weak<ContextInner>,
}

struct TargetResources {
    shared: Arc<DeviceShared>,
    key: TargetKey,
    allocation_id: u64,
    image: vk::Image,
    image_view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_allocation_size: vk::DeviceSize,
    readback_buffer: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_allocation_size: vk::DeviceSize,
    readback_host_coherent: bool,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    operation: Mutex<TargetOperation>,
}

struct TargetOperation {
    layout: vk::ImageLayout,
    in_flight: bool,
}

#[derive(Clone, Copy)]
struct PassSynchronization {
    layout: vk::ImageLayout,
    destination_stage: vk::PipelineStageFlags,
    destination_access: vk::AccessFlags,
    source_stage: vk::PipelineStageFlags,
    source_access: vk::AccessFlags,
}

#[derive(Clone, Copy, Debug, Default)]
struct SubmissionTimings {
    command_record_us: u64,
    queue_submit_us: u64,
}

struct TargetAllocationGuard {
    shared: Arc<DeviceShared>,
    image: vk::Image,
    image_view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    readback_buffer: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    command_pool: vk::CommandPool,
    fence: vk::Fence,
    armed: bool,
}

impl VulkanOffscreenContext {
    /// Create a retained no-window Vulkan context with the default bounded cache.
    pub fn new() -> Result<Self, VulkanOffscreenError> {
        Self::with_target_cache_capacity(DEFAULT_TARGET_CACHE_CAPACITY)
    }

    /// Create a retained no-window context with an explicit target-cache bound.
    pub fn with_target_cache_capacity(capacity: usize) -> Result<Self, VulkanOffscreenError> {
        let _runtime_span = tracing::info_span!(
            "vulkan_offscreen_context_create",
            target_cache_capacity = capacity
        )
        .entered();
        Self::with_entry_loader(capacity, || {
            // SAFETY: Loading the process Vulkan entry points does not access
            // application-owned memory. The Entry is retained for the full
            // instance lifetime.
            unsafe { Entry::load() }.map_err(|error| VulkanOffscreenError::LoaderUnavailable {
                detail: error.to_string(),
            })
        })
    }

    /// Probe availability without ever creating a native window or surface.
    #[must_use]
    pub fn probe() -> VulkanOffscreenAvailability {
        let _availability_span = tracing::info_span!("vulkan_offscreen_availability").entered();
        match Self::new() {
            Ok(context) => VulkanOffscreenAvailability::Available(context.device_facts().clone()),
            Err(error) => VulkanOffscreenAvailability::Unavailable(
                VulkanOffscreenUnavailable::from_error(&error),
            ),
        }
    }

    fn with_entry_loader<F>(capacity: usize, loader: F) -> Result<Self, VulkanOffscreenError>
    where
        F: FnOnce() -> Result<Entry, VulkanOffscreenError>,
    {
        if capacity == 0 {
            return Err(VulkanOffscreenError::InvalidCacheCapacity);
        }
        let entry = loader()?;
        Self::from_entry(entry, capacity)
    }

    fn from_entry(entry: Entry, capacity: usize) -> Result<Self, VulkanOffscreenError> {
        let application_info = vk::ApplicationInfo::default()
            .application_name(c"teamy-terminal-offscreen")
            .application_version(1)
            .engine_name(c"teamy-terminal")
            .engine_version(1)
            .api_version(vk::API_VERSION_1_0);
        // Intentionally no extension names: this runtime must not require a
        // platform surface, raw handles, or a swapchain.
        let instance_info = vk::InstanceCreateInfo::default().application_info(&application_info);
        // SAFETY: The create-info references static C strings and enables no
        // extension pointers. Entry outlives the returned instance.
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|source| VulkanOffscreenError::InstanceCreation { source })?;

        let selection = match select_physical_device(&instance) {
            Ok(selection) => selection,
            Err(error) => {
                // SAFETY: The instance is live and no children were created.
                unsafe { instance.destroy_instance(None) };
                return Err(error);
            }
        };
        let queue_priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(selection.queue_family_index)
            .queue_priorities(&queue_priorities)];
        // Intentionally no device extensions: transfer operations are Vulkan
        // core functionality and do not require VK_KHR_swapchain.
        let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_info);
        // SAFETY: The queue family came from this physical device and one queue
        // priority is provided for the requested queue.
        let device = match unsafe {
            instance.create_device(selection.physical_device, &device_info, None)
        } {
            Ok(device) => device,
            Err(source) => {
                // SAFETY: The instance is live and no logical device exists.
                unsafe { instance.destroy_instance(None) };
                return Err(VulkanOffscreenError::DeviceCreation { source });
            }
        };
        // SAFETY: Queue zero exists because the device create-info requested it.
        let queue = unsafe { device.get_device_queue(selection.queue_family_index, 0) };
        // SAFETY: The physical-device handle belongs to this live instance.
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(selection.physical_device) };
        let shared = Arc::new(DeviceShared {
            _entry: entry,
            instance,
            physical_device: selection.physical_device,
            device,
            queue_family_index: selection.queue_family_index,
            queue,
            queue_submit: Mutex::new(()),
            memory_properties,
            facts: selection.facts,
            device_lost: AtomicBool::new(false),
            allocations: AtomicU64::new(0),
            destructions: AtomicU64::new(0),
        });
        Ok(Self {
            inner: Arc::new(ContextInner {
                cache: Mutex::new(ContextCache {
                    capacity,
                    tick: 0,
                    entries: HashMap::new(),
                    reuses: 0,
                    evictions: 0,
                }),
                device: shared,
                next_session_id: AtomicU64::new(1),
                next_allocation_id: AtomicU64::new(1),
            }),
        })
    }

    /// Create an independent terminal/session target namespace.
    #[must_use]
    pub fn create_session(&self) -> VulkanOffscreenSession {
        let id = self.inner.next_session_id.fetch_add(1, Ordering::Relaxed);
        VulkanOffscreenSession {
            lease: Arc::new(SessionLease {
                id,
                context: Arc::downgrade(&self.inner),
            }),
        }
    }

    /// Selected no-window device facts, including the empty extension sets.
    #[must_use]
    pub fn device_facts(&self) -> &VulkanDeviceFacts {
        &self.inner.device.facts
    }

    /// Borrow the retained device for renderer-crate pipeline construction.
    ///
    /// This is intentionally crate-internal: external consumers select a
    /// renderer and receive pixels, never raw Vulkan handles.
    pub(crate) fn device_access(&self) -> VulkanOffscreenDeviceAccess<'_> {
        VulkanOffscreenDeviceAccess {
            instance: &self.inner.device.instance,
            physical_device: self.inner.device.physical_device,
            device: &self.inner.device.device,
            queue_family_index: self.inner.device.queue_family_index,
        }
    }

    /// Whether Vulkan has reported device loss for this retained context.
    #[must_use]
    pub fn is_device_lost(&self) -> bool {
        self.inner.device.device_lost.load(Ordering::Acquire)
    }

    /// Current allocation/reuse/eviction/destruction counters.
    #[must_use]
    pub fn cache_stats(&self) -> VulkanOffscreenCacheStats {
        let cache = self
            .inner
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        VulkanOffscreenCacheStats {
            capacity: cache.capacity,
            retained_targets: cache.entries.len(),
            allocations: self.inner.device.allocations.load(Ordering::Acquire),
            reuses: cache.reuses,
            evictions: cache.evictions,
            destructions: self.inner.device.destructions.load(Ordering::Acquire),
        }
    }

    fn acquire_target(
        &self,
        key: TargetKey,
    ) -> Result<VulkanOffscreenTarget, VulkanOffscreenError> {
        self.validate_extent(key.width, key.height)?;
        if self.is_device_lost() {
            return Err(VulkanOffscreenError::DeviceLost {
                operation: "target acquisition",
            });
        }

        if let Some(target) = self.cached_target(key) {
            return Ok(VulkanOffscreenTarget { resources: target });
        }

        let allocation_id = self
            .inner
            .next_allocation_id
            .fetch_add(1, Ordering::Relaxed);
        let created = Arc::new(TargetResources::create(
            Arc::clone(&self.inner.device),
            key,
            allocation_id,
        )?);

        let mut discarded = None;
        let mut evicted = None;
        let selected = {
            let mut cache = self
                .inner
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.tick = cache.tick.wrapping_add(1);
            let tick = cache.tick;
            if let Some(existing) = cache.entries.get_mut(&key) {
                existing.last_used = tick;
                let target = Arc::clone(&existing.target);
                cache.reuses = cache.reuses.saturating_add(1);
                discarded = Some(created);
                target
            } else {
                if cache.entries.len() >= cache.capacity {
                    let oldest = cache
                        .entries
                        .iter()
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(key, _)| *key);
                    if let Some(oldest) = oldest {
                        evicted = cache.entries.remove(&oldest).map(|entry| entry.target);
                        cache.evictions = cache.evictions.saturating_add(1);
                    }
                }
                cache.entries.insert(
                    key,
                    CacheEntry {
                        target: Arc::clone(&created),
                        last_used: tick,
                    },
                );
                created
            }
        };
        // Resource destruction can wait for in-flight work, so never do it
        // while holding the cache lock.
        drop(discarded);
        drop(evicted);
        Ok(VulkanOffscreenTarget {
            resources: selected,
        })
    }

    fn cached_target(&self, key: TargetKey) -> Option<Arc<TargetResources>> {
        let mut cache = self
            .inner
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.tick = cache.tick.wrapping_add(1);
        let tick = cache.tick;
        let target = cache.entries.get_mut(&key).map(|entry| {
            entry.last_used = tick;
            Arc::clone(&entry.target)
        });
        if target.is_some() {
            cache.reuses = cache.reuses.saturating_add(1);
        }
        target
    }

    fn validate_extent(&self, width: u32, height: u32) -> Result<(), VulkanOffscreenError> {
        if width == 0 || height == 0 {
            return Err(VulkanOffscreenError::InvalidExtent {
                width,
                height,
                detail: "both dimensions must be non-zero".to_owned(),
            });
        }
        let limit = self.inner.device.facts.max_image_dimension_2d;
        if width > limit || height > limit {
            return Err(VulkanOffscreenError::InvalidExtent {
                width,
                height,
                detail: format!("device limit is {limit} pixels per dimension"),
            });
        }
        checked_byte_len(width, height).map(|_| ())
    }
}

impl VulkanOffscreenAvailability {
    #[cfg(test)]
    fn from_result(result: Result<VulkanOffscreenContext, VulkanOffscreenError>) -> Self {
        match result {
            Ok(context) => Self::Available(context.device_facts().clone()),
            Err(error) => Self::Unavailable(VulkanOffscreenUnavailable::from_error(&error)),
        }
    }
}

impl VulkanOffscreenSession {
    /// Stable process-local session identifier used in target cache keys.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.lease.id
    }

    /// Acquire or reuse an exact-size RGBA8 target for this session.
    pub fn target(
        &self,
        width: u32,
        height: u32,
    ) -> Result<VulkanOffscreenTarget, VulkanOffscreenError> {
        let context = self
            .lease
            .context
            .upgrade()
            .ok_or(VulkanOffscreenError::Operation {
                operation: "session target acquisition after context shutdown",
                source: vk::Result::ERROR_INITIALIZATION_FAILED,
            })?;
        VulkanOffscreenContext { inner: context }.acquire_target(TargetKey {
            session_id: self.lease.id,
            width,
            height,
        })
    }

    /// Evict all cached targets owned by this session.
    pub fn clear_cached_targets(&self) {
        if let Some(context) = self.lease.context.upgrade() {
            remove_session_targets(&context, self.lease.id);
        }
    }
}

impl VulkanOffscreenTarget {
    /// Stable allocation identity useful for proving cache reuse.
    #[must_use]
    pub fn allocation_id(&self) -> u64 {
        self.resources.allocation_id
    }

    /// Target width.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.resources.key.width
    }

    /// Target height.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.resources.key.height
    }

    /// Bytes retained in the exact-size host-visible staging allocation.
    #[must_use]
    pub fn staging_capacity_bytes(&self) -> vk::DeviceSize {
        self.resources.staging_allocation_size
    }

    /// Clear the GPU image and synchronously read back deterministic RGBA8 bytes.
    pub fn clear_and_readback(
        &self,
        color: [f32; 4],
        timeout: Duration,
    ) -> Result<VulkanOffscreenReadback, VulkanOffscreenError> {
        self.clear_and_readback_cancellable(color, timeout, None)
    }

    /// Clear/read back with cooperative cancellation while retaining safe GPU ownership.
    pub fn clear_and_readback_cancellable(
        &self,
        color: [f32; 4],
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
    ) -> Result<VulkanOffscreenReadback, VulkanOffscreenError> {
        self.execute_pass_and_readback(
            PassSynchronization {
                layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                destination_stage: vk::PipelineStageFlags::TRANSFER,
                destination_access: vk::AccessFlags::TRANSFER_WRITE,
                source_stage: vk::PipelineStageFlags::TRANSFER,
                source_access: vk::AccessFlags::TRANSFER_WRITE,
            },
            timeout,
            cancellation,
            |recording| {
                let clear = vk::ClearColorValue { float32: color };
                // SAFETY: The pass owner transitioned the exclusively-held
                // image to TRANSFER_DST_OPTIMAL before invoking this callback.
                unsafe {
                    recording.device().cmd_clear_color_image(
                        recording.command_buffer(),
                        recording.image(),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &clear,
                        &[color_subresource_range()],
                    )
                };
                Ok(())
            },
        )
    }

    /// Record a compute pass into this retained target and read it back.
    ///
    /// This crate-internal seam is reserved for the sibling slug module. The
    /// callback receives an image in `GENERAL` layout and records compute work
    /// only; this runtime owns command/fence reset, barriers, queue submission,
    /// timeout/cancellation, and transfer-to-host readback.
    pub(crate) fn record_compute_and_readback<F>(
        &self,
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
        record: F,
    ) -> Result<VulkanOffscreenReadback, VulkanOffscreenError>
    where
        F: FnOnce(&VulkanOffscreenRecording<'_>) -> Result<(), VulkanOffscreenError>,
    {
        self.execute_pass_and_readback(
            PassSynchronization {
                layout: vk::ImageLayout::GENERAL,
                destination_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                destination_access: vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                source_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                source_access: vk::AccessFlags::SHADER_WRITE,
            },
            timeout,
            cancellation,
            record,
        )
    }

    /// Record compute work and copy only the requested tight regions to host memory.
    ///
    /// No full-image GPU-to-host transfer occurs on this path. Regions are
    /// validated, ordered as supplied, and packed contiguously in the retained
    /// readback allocation.
    pub(crate) fn record_compute_and_readback_regions<F>(
        &self,
        regions: &[VulkanOffscreenRegion],
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
        record: F,
    ) -> Result<VulkanOffscreenRegionalReadback, VulkanOffscreenError>
    where
        F: FnOnce(&VulkanOffscreenRecording<'_>) -> Result<(), VulkanOffscreenError>,
    {
        self.execute_pass_and_readback_regions(
            PassSynchronization {
                layout: vk::ImageLayout::GENERAL,
                destination_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                destination_access: vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                source_stage: vk::PipelineStageFlags::COMPUTE_SHADER,
                source_access: vk::AccessFlags::SHADER_WRITE,
            },
            regions,
            timeout,
            cancellation,
            record,
        )
    }

    fn execute_pass_and_readback<F>(
        &self,
        synchronization: PassSynchronization,
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
        record: F,
    ) -> Result<VulkanOffscreenReadback, VulkanOffscreenError>
    where
        F: FnOnce(&VulkanOffscreenRecording<'_>) -> Result<(), VulkanOffscreenError>,
    {
        let full = [VulkanOffscreenRegion {
            x: 0,
            y: 0,
            width: self.width(),
            height: self.height(),
        }];
        let regional = self.execute_pass_and_readback_regions(
            synchronization,
            &full,
            timeout,
            cancellation,
            record,
        )?;
        let region =
            regional
                .regions
                .into_iter()
                .next()
                .ok_or(VulkanOffscreenError::Operation {
                    operation: "full readback produced no region",
                    source: vk::Result::ERROR_INITIALIZATION_FAILED,
                })?;
        Ok(VulkanOffscreenReadback {
            width: regional.frame_width,
            height: regional.frame_height,
            rgba: region.rgba,
            timings: regional.timings,
        })
    }

    fn execute_pass_and_readback_regions<F>(
        &self,
        synchronization: PassSynchronization,
        regions: &[VulkanOffscreenRegion],
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
        record: F,
    ) -> Result<VulkanOffscreenRegionalReadback, VulkanOffscreenError>
    where
        F: FnOnce(&VulkanOffscreenRecording<'_>) -> Result<(), VulkanOffscreenError>,
    {
        if cancellation.is_some_and(VulkanOffscreenCancellation::is_cancelled) {
            return Err(VulkanOffscreenError::Cancelled);
        }
        if self.resources.shared.device_lost.load(Ordering::Acquire) {
            return Err(VulkanOffscreenError::DeviceLost {
                operation: "clear/readback",
            });
        }
        self.resources.validate_readback_regions(regions)?;
        let mut operation = self
            .resources
            .operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut gpu_completion_wait_us = 0_u64;
        if operation.in_flight {
            gpu_completion_wait_us = gpu_completion_wait_us.saturating_add(
                self.resources
                    .wait_for_completion(&mut operation, timeout, cancellation)?,
            );
        }
        let submission = self.resources.record_pass_and_submit(
            &mut operation,
            synchronization,
            regions,
            record,
        )?;
        gpu_completion_wait_us = gpu_completion_wait_us.saturating_add(
            self.resources
                .wait_for_completion(&mut operation, timeout, cancellation)?,
        );
        let readback_start = Instant::now();
        let readback = self.resources.readback_regions(regions)?;
        Ok(VulkanOffscreenRegionalReadback {
            frame_width: self.width(),
            frame_height: self.height(),
            regions: readback,
            timings: VulkanOffscreenStageTimings {
                command_record_us: submission.command_record_us,
                queue_submit_us: submission.queue_submit_us,
                gpu_completion_wait_us,
                readback_map_copy_us: elapsed_micros(readback_start),
            },
        })
    }
}

impl SessionLease {
    fn remove_cached_targets(&self) {
        if let Some(context) = self.context.upgrade() {
            remove_session_targets(&context, self.id);
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.remove_cached_targets();
    }
}

impl TargetResources {
    fn create(
        shared: Arc<DeviceShared>,
        key: TargetKey,
        allocation_id: u64,
    ) -> Result<Self, VulkanOffscreenError> {
        let byte_len = checked_byte_len(key.width, key.height)?;
        let mut guard = TargetAllocationGuard::new(Arc::clone(&shared));
        let extent = vk::Extent3D {
            width: key.width,
            height: key.height,
            depth: 1,
        };
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(OFFSCREEN_FORMAT)
            .extent(extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::STORAGE,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: The device is live and the image create-info is complete.
        guard.image = unsafe { shared.device.create_image(&image_info, None) }
            .map_err(|source| allocation_error(&shared, "offscreen image", source))?;
        // SAFETY: The image was created by this device and is not yet bound.
        let image_requirements =
            unsafe { shared.device.get_image_memory_requirements(guard.image) };
        let image_memory_type = shared
            .find_memory_type(
                image_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .ok_or_else(|| VulkanOffscreenError::NoCompatibleDevice {
                detail: "no memory type can back the offscreen RGBA8 image".to_owned(),
            })?;
        let image_allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(image_requirements.size)
            .memory_type_index(image_memory_type.0);
        // SAFETY: The memory type is compatible with the image requirements.
        guard.image_memory = unsafe { shared.device.allocate_memory(&image_allocate, None) }
            .map_err(|source| allocation_error(&shared, "offscreen image memory", source))?;
        // SAFETY: The allocation satisfies this image's memory requirements.
        unsafe {
            shared
                .device
                .bind_image_memory(guard.image, guard.image_memory, 0)
        }
        .map_err(|source| operation_error(&shared, "image-memory bind", source))?;
        let image_view_info = vk::ImageViewCreateInfo::default()
            .image(guard.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(OFFSCREEN_FORMAT)
            .subresource_range(color_subresource_range());
        // SAFETY: The image is live, bound, and has the matching RGBA8 format.
        guard.image_view = unsafe { shared.device.create_image_view(&image_view_info, None) }
            .map_err(|source| allocation_error(&shared, "offscreen image view", source))?;

        let staging_info = vk::BufferCreateInfo::default()
            .size(byte_len)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: The device is live and the exact-size staging create-info is complete.
        guard.staging_buffer = unsafe { shared.device.create_buffer(&staging_info, None) }
            .map_err(|source| allocation_error(&shared, "staging buffer", source))?;
        // SAFETY: The buffer was created by this device and is not yet bound.
        let staging_requirements = unsafe {
            shared
                .device
                .get_buffer_memory_requirements(guard.staging_buffer)
        };
        let staging_memory_type = shared
            .find_memory_type(
                staging_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED,
            )
            .ok_or_else(|| VulkanOffscreenError::NoCompatibleDevice {
                detail: "no host-visible memory type can back the staging buffer".to_owned(),
            })?;
        let staging_allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(staging_requirements.size)
            .memory_type_index(staging_memory_type.0);
        // SAFETY: The selected memory type is compatible and host-visible.
        guard.staging_memory = unsafe { shared.device.allocate_memory(&staging_allocate, None) }
            .map_err(|source| allocation_error(&shared, "staging memory", source))?;
        // SAFETY: The allocation satisfies this staging buffer's requirements.
        unsafe {
            shared
                .device
                .bind_buffer_memory(guard.staging_buffer, guard.staging_memory, 0)
        }
        .map_err(|source| operation_error(&shared, "staging-memory bind", source))?;

        let buffer_info = vk::BufferCreateInfo::default()
            .size(byte_len)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: The device is live and the buffer create-info is complete.
        guard.readback_buffer = unsafe { shared.device.create_buffer(&buffer_info, None) }
            .map_err(|source| allocation_error(&shared, "readback buffer", source))?;
        // SAFETY: The buffer was created by this device and is not yet bound.
        let buffer_requirements = unsafe {
            shared
                .device
                .get_buffer_memory_requirements(guard.readback_buffer)
        };
        let (buffer_memory_type, buffer_memory_flags) = shared
            .find_memory_type(
                buffer_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE,
                vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED,
            )
            .ok_or_else(|| VulkanOffscreenError::NoCompatibleDevice {
                detail: "no host-visible memory type can back the readback buffer".to_owned(),
            })?;
        let buffer_allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(buffer_requirements.size)
            .memory_type_index(buffer_memory_type);
        // SAFETY: The memory type is compatible and host-visible.
        guard.readback_memory = unsafe { shared.device.allocate_memory(&buffer_allocate, None) }
            .map_err(|source| allocation_error(&shared, "readback memory", source))?;
        // SAFETY: The allocation satisfies this buffer's memory requirements.
        unsafe {
            shared
                .device
                .bind_buffer_memory(guard.readback_buffer, guard.readback_memory, 0)
        }
        .map_err(|source| operation_error(&shared, "buffer-memory bind", source))?;

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(shared.queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: The queue-family index belongs to the logical device.
        guard.command_pool = unsafe { shared.device.create_command_pool(&pool_info, None) }
            .map_err(|source| allocation_error(&shared, "command pool", source))?;
        let command_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(guard.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: The retained command pool is live and resettable.
        let command_buffer = unsafe { shared.device.allocate_command_buffers(&command_info) }
            .map_err(|source| allocation_error(&shared, "command buffer", source))?
            .into_iter()
            .next()
            .ok_or(VulkanOffscreenError::Allocation {
                resource: "command buffer",
                source: vk::Result::ERROR_INITIALIZATION_FAILED,
            })?;
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: The device is live and the fence create-info is complete.
        guard.fence = unsafe { shared.device.create_fence(&fence_info, None) }
            .map_err(|source| allocation_error(&shared, "completion fence", source))?;

        shared.allocations.fetch_add(1, Ordering::AcqRel);
        guard.armed = false;
        Ok(Self {
            shared,
            key,
            allocation_id,
            image: guard.image,
            image_view: guard.image_view,
            image_memory: guard.image_memory,
            staging_buffer: guard.staging_buffer,
            staging_memory: guard.staging_memory,
            staging_allocation_size: staging_requirements.size,
            readback_buffer: guard.readback_buffer,
            readback_memory: guard.readback_memory,
            readback_allocation_size: buffer_requirements.size,
            readback_host_coherent: buffer_memory_flags
                .contains(vk::MemoryPropertyFlags::HOST_COHERENT),
            command_pool: guard.command_pool,
            command_buffer,
            fence: guard.fence,
            operation: Mutex::new(TargetOperation {
                layout: vk::ImageLayout::UNDEFINED,
                in_flight: false,
            }),
        })
    }

    fn record_pass_and_submit<F>(
        &self,
        operation: &mut TargetOperation,
        synchronization: PassSynchronization,
        readback_regions: &[VulkanOffscreenRegion],
        record: F,
    ) -> Result<SubmissionTimings, VulkanOffscreenError>
    where
        F: FnOnce(&VulkanOffscreenRecording<'_>) -> Result<(), VulkanOffscreenError>,
    {
        let command_record_start = Instant::now();
        // SAFETY: The fence belongs to this device and any previous work was
        // waited before this method is called.
        unsafe { self.shared.device.reset_fences(&[self.fence]) }
            .map_err(|source| operation_error(&self.shared, "fence reset", source))?;
        // SAFETY: This target serializes host access to its command buffer.
        unsafe {
            self.shared
                .device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
        }
        .map_err(|source| operation_error(&self.shared, "command-buffer reset", source))?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: The command buffer is reset, idle, and exclusively held.
        unsafe {
            self.shared
                .device
                .begin_command_buffer(self.command_buffer, &begin)
        }
        .map_err(|source| operation_error(&self.shared, "command-buffer begin", source))?;

        let (source_stage, source_access) = if operation.layout == vk::ImageLayout::UNDEFINED {
            (
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::AccessFlags::empty(),
            )
        } else {
            (
                vk::PipelineStageFlags::TRANSFER,
                vk::AccessFlags::TRANSFER_READ,
            )
        };
        let to_pass = vk::ImageMemoryBarrier::default()
            .src_access_mask(source_access)
            .dst_access_mask(synchronization.destination_access)
            .old_layout(operation.layout)
            .new_layout(synchronization.layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(color_subresource_range());
        // SAFETY: The target command buffer and image are exclusively held;
        // the barrier describes the tracked previous layout.
        unsafe {
            self.shared.device.cmd_pipeline_barrier(
                self.command_buffer,
                source_stage,
                synchronization.destination_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_pass],
            )
        };
        let recording = VulkanOffscreenRecording {
            device: &self.shared.device,
            command_buffer: self.command_buffer,
            image: self.image,
            image_view: self.image_view,
            extent: vk::Extent2D {
                width: self.key.width,
                height: self.key.height,
            },
        };
        record(&recording)?;
        let to_copy = vk::ImageMemoryBarrier::default()
            .src_access_mask(synchronization.source_access)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(synchronization.layout)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(color_subresource_range());
        // SAFETY: The barrier orders the clear before the image-to-buffer copy.
        unsafe {
            self.shared.device.cmd_pipeline_barrier(
                self.command_buffer,
                synchronization.source_stage,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_copy],
            )
        };
        let subresource = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .mip_level(0)
            .base_array_layer(0)
            .layer_count(1);
        let mut buffer_offset = 0_u64;
        let copies = readback_regions
            .iter()
            .map(|region| {
                let copy = vk::BufferImageCopy::default()
                    .buffer_offset(buffer_offset)
                    .buffer_row_length(0)
                    .buffer_image_height(0)
                    .image_subresource(subresource)
                    .image_offset(vk::Offset3D {
                        x: i32::try_from(region.x).unwrap_or(i32::MAX),
                        y: i32::try_from(region.y).unwrap_or(i32::MAX),
                        z: 0,
                    })
                    .image_extent(vk::Extent3D {
                        width: region.width,
                        height: region.height,
                        depth: 1,
                    });
                buffer_offset = buffer_offset.saturating_add(
                    u64::from(region.width)
                        .saturating_mul(u64::from(region.height))
                        .saturating_mul(4),
                );
                copy
            })
            .collect::<Vec<_>>();
        // SAFETY: The image is transfer-source, the buffer is large enough,
        // and both resources are exclusively owned by this target operation.
        unsafe {
            self.shared.device.cmd_copy_image_to_buffer(
                self.command_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback_buffer,
                &copies,
            )
        };
        let host_read = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.readback_buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        // SAFETY: This makes the completed transfer visible to host reads.
        unsafe {
            self.shared.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[],
                &[host_read],
                &[],
            )
        };
        // SAFETY: The command buffer is recording and all referenced resources
        // remain live until its fence completes.
        unsafe { self.shared.device.end_command_buffer(self.command_buffer) }
            .map_err(|source| operation_error(&self.shared, "command-buffer end", source))?;
        let command_record_us = elapsed_micros(command_record_start);
        let command_buffers = [self.command_buffer];
        let submit = [vk::SubmitInfo::default().command_buffers(&command_buffers)];
        let queue_submit_start = Instant::now();
        let queue_guard = self
            .shared
            .queue_submit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: Queue submission is externally synchronized and the command
        // buffer/fence are idle and exclusively owned by this target.
        let submit_result = unsafe {
            self.shared
                .device
                .queue_submit(self.shared.queue, &submit, self.fence)
        };
        drop(queue_guard);
        submit_result.map_err(|source| operation_error(&self.shared, "queue submit", source))?;
        operation.in_flight = true;
        operation.layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
        Ok(SubmissionTimings {
            command_record_us,
            queue_submit_us: elapsed_micros(queue_submit_start),
        })
    }

    fn wait_for_completion(
        &self,
        operation: &mut TargetOperation,
        timeout: Duration,
        cancellation: Option<&VulkanOffscreenCancellation>,
    ) -> Result<u64, VulkanOffscreenError> {
        let wait_start = Instant::now();
        let wait_result = poll_until_complete(timeout, cancellation, |wait_ns| {
            // SAFETY: The fence is live. Polling does not mutate application memory.
            unsafe {
                self.shared
                    .device
                    .wait_for_fences(&[self.fence], true, wait_ns)
            }
        });
        match wait_result {
            Ok(()) => {
                operation.in_flight = false;
                Ok(elapsed_micros(wait_start))
            }
            Err(VulkanOffscreenError::Operation { operation, source }) => {
                Err(operation_error(&self.shared, operation, source))
            }
            Err(error) => Err(error),
        }
    }

    fn validate_readback_regions(
        &self,
        regions: &[VulkanOffscreenRegion],
    ) -> Result<(), VulkanOffscreenError> {
        if regions.is_empty() || regions.len() > 64 {
            return Err(VulkanOffscreenError::InvalidExtent {
                width: self.key.width,
                height: self.key.height,
                detail: "regional readback requires 1..=64 regions".to_owned(),
            });
        }
        let mut total = 0_u64;
        for region in regions {
            if region.width == 0
                || region.height == 0
                || region.x.saturating_add(region.width) > self.key.width
                || region.y.saturating_add(region.height) > self.key.height
                || region.x > i32::MAX as u32
                || region.y > i32::MAX as u32
            {
                return Err(VulkanOffscreenError::InvalidExtent {
                    width: region.width,
                    height: region.height,
                    detail: format!(
                        "readback region ({},{}) {}x{} exceeds target {}x{}",
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        self.key.width,
                        self.key.height,
                    ),
                });
            }
            total = total
                .checked_add(
                    u64::from(region.width)
                        .checked_mul(u64::from(region.height))
                        .and_then(|pixels| pixels.checked_mul(4))
                        .ok_or(VulkanOffscreenError::InvalidExtent {
                            width: region.width,
                            height: region.height,
                            detail: "regional RGBA byte length overflows".to_owned(),
                        })?,
                )
                .ok_or(VulkanOffscreenError::InvalidExtent {
                    width: self.key.width,
                    height: self.key.height,
                    detail: "combined regional RGBA byte length overflows".to_owned(),
                })?;
        }
        if total > self.readback_allocation_size {
            return Err(VulkanOffscreenError::InvalidExtent {
                width: self.key.width,
                height: self.key.height,
                detail: "combined regional readback exceeds retained allocation".to_owned(),
            });
        }
        Ok(())
    }

    fn readback_regions(
        &self,
        regions: &[VulkanOffscreenRegion],
    ) -> Result<Vec<VulkanOffscreenRegionReadback>, VulkanOffscreenError> {
        // SAFETY: The memory is host-visible, GPU writes are complete, and the
        // requested range lies inside the allocation.
        let mapped = unsafe {
            self.shared.device.map_memory(
                self.readback_memory,
                0,
                self.readback_allocation_size,
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(|source| operation_error(&self.shared, "readback map", source))?;
        if !self.readback_host_coherent {
            let range = vk::MappedMemoryRange::default()
                .memory(self.readback_memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            // SAFETY: GPU work is complete and the range covers this target's
            // host-visible readback allocation.
            unsafe { self.shared.device.invalidate_mapped_memory_ranges(&[range]) }.map_err(
                |source| {
                    // SAFETY: map_memory above succeeded and this target serializes mapping.
                    unsafe { self.shared.device.unmap_memory(self.readback_memory) };
                    operation_error(&self.shared, "readback invalidate", source)
                },
            )?;
        }
        let mut offset = 0_usize;
        let mut output = Vec::with_capacity(regions.len());
        for region in regions {
            let byte_len = usize::try_from(u64::from(region.width) * u64::from(region.height) * 4)
                .map_err(|_| VulkanOffscreenError::InvalidExtent {
                    width: region.width,
                    height: region.height,
                    detail: "regional readback byte length exceeds host address space".to_owned(),
                })?;
            // SAFETY: Validation proved the packed offset/range lies in the mapped allocation.
            let rgba =
                unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(offset), byte_len) }
                    .to_vec();
            output.push(VulkanOffscreenRegionReadback {
                region: *region,
                rgba,
            });
            offset = offset.saturating_add(byte_len);
        }
        // SAFETY: The allocation is currently mapped exactly once by this serialized target.
        unsafe { self.shared.device.unmap_memory(self.readback_memory) };
        Ok(output)
    }
}

impl Drop for TargetResources {
    fn drop(&mut self) {
        let operation = self
            .operation
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if operation.in_flight && !self.shared.device_lost.load(Ordering::Acquire) {
            // SAFETY: This target owns the fence and keeps every submitted
            // resource alive until completion. Drop must not free in-flight work.
            let _ = unsafe {
                self.shared
                    .device
                    .wait_for_fences(&[self.fence], true, u64::MAX)
            };
        }
        // SAFETY: Drop runs exactly once, after in-flight work completes (or
        // device loss), and all handles belong to this live device.
        unsafe {
            self.shared.device.destroy_fence(self.fence, None);
            self.shared
                .device
                .destroy_command_pool(self.command_pool, None);
            self.shared
                .device
                .destroy_buffer(self.readback_buffer, None);
            self.shared.device.free_memory(self.readback_memory, None);
            self.shared.device.destroy_buffer(self.staging_buffer, None);
            self.shared.device.free_memory(self.staging_memory, None);
            self.shared.device.destroy_image_view(self.image_view, None);
            self.shared.device.destroy_image(self.image, None);
            self.shared.device.free_memory(self.image_memory, None);
        }
        self.shared.destructions.fetch_add(1, Ordering::AcqRel);
    }
}

impl TargetAllocationGuard {
    fn new(shared: Arc<DeviceShared>) -> Self {
        Self {
            shared,
            image: vk::Image::null(),
            image_view: vk::ImageView::null(),
            image_memory: vk::DeviceMemory::null(),
            staging_buffer: vk::Buffer::null(),
            staging_memory: vk::DeviceMemory::null(),
            readback_buffer: vk::Buffer::null(),
            readback_memory: vk::DeviceMemory::null(),
            command_pool: vk::CommandPool::null(),
            fence: vk::Fence::null(),
            armed: true,
        }
    }
}

impl Drop for TargetAllocationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: Every non-null handle was created by this device and no work
        // has been submitted with these partially-built resources.
        unsafe {
            if self.fence != vk::Fence::null() {
                self.shared.device.destroy_fence(self.fence, None);
            }
            if self.command_pool != vk::CommandPool::null() {
                self.shared
                    .device
                    .destroy_command_pool(self.command_pool, None);
            }
            if self.readback_buffer != vk::Buffer::null() {
                self.shared
                    .device
                    .destroy_buffer(self.readback_buffer, None);
            }
            if self.readback_memory != vk::DeviceMemory::null() {
                self.shared.device.free_memory(self.readback_memory, None);
            }
            if self.staging_buffer != vk::Buffer::null() {
                self.shared.device.destroy_buffer(self.staging_buffer, None);
            }
            if self.staging_memory != vk::DeviceMemory::null() {
                self.shared.device.free_memory(self.staging_memory, None);
            }
            if self.image != vk::Image::null() {
                if self.image_view != vk::ImageView::null() {
                    self.shared.device.destroy_image_view(self.image_view, None);
                }
                self.shared.device.destroy_image(self.image, None);
            }
            if self.image_memory != vk::DeviceMemory::null() {
                self.shared.device.free_memory(self.image_memory, None);
            }
        }
    }
}

impl DeviceShared {
    fn find_memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Option<(u32, vk::MemoryPropertyFlags)> {
        let mut fallback = None;
        for (index, memory_type) in self.memory_properties.memory_types.iter().enumerate() {
            let index = u32::try_from(index).ok()?;
            if (type_bits & (1_u32 << index)) == 0 || !memory_type.property_flags.contains(required)
            {
                continue;
            }
            let candidate = (index, memory_type.property_flags);
            if memory_type.property_flags.contains(preferred) {
                return Some(candidate);
            }
            fallback.get_or_insert(candidate);
        }
        fallback
    }
}

impl Drop for DeviceShared {
    fn drop(&mut self) {
        if !self.device_lost.load(Ordering::Acquire) {
            // SAFETY: This is the final shared device owner, after all cached
            // and leased targets have released their resources.
            let _ = unsafe { self.device.device_wait_idle() };
        }
        // SAFETY: The logical device and instance are live and destroyed once,
        // in parent-before-entry order.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn remove_session_targets(context: &ContextInner, session_id: u64) {
    let removed = {
        let mut cache = context
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys = cache
            .entries
            .keys()
            .copied()
            .filter(|key| key.session_id == session_id)
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| cache.entries.remove(&key).map(|entry| entry.target))
            .collect::<Vec<_>>()
    };
    drop(removed);
}

struct DeviceSelection {
    physical_device: vk::PhysicalDevice,
    queue_family_index: u32,
    facts: VulkanDeviceFacts,
}

fn select_physical_device(
    instance: &ash::Instance,
) -> Result<DeviceSelection, VulkanOffscreenError> {
    // SAFETY: The Vulkan instance is live for the duration of device probing.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|source| VulkanOffscreenError::DeviceEnumeration { source })?;
    let mut rejection_reasons = Vec::new();
    for physical_device in devices {
        // SAFETY: The physical-device handle belongs to this instance.
        let properties = unsafe { instance.get_physical_device_properties(physical_device) };
        let device_name = device_name(&properties);
        // SAFETY: The physical-device handle belongs to this instance.
        let format = unsafe {
            instance.get_physical_device_format_properties(physical_device, OFFSCREEN_FORMAT)
        };
        let required_format = vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::TRANSFER_DST
            | vk::FormatFeatureFlags::STORAGE_IMAGE;
        if !format.optimal_tiling_features.contains(required_format) {
            rejection_reasons.push(format!(
                "{device_name}: RGBA8 optimal images lack storage/transfer src/dst"
            ));
            continue;
        }
        // SAFETY: The physical-device handle belongs to this instance.
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let Some((queue_family_index, family)) = families.iter().enumerate().find(|(_, family)| {
            family.queue_count > 0 && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        }) else {
            rejection_reasons.push(format!("{device_name}: no graphics queue family"));
            continue;
        };
        let queue_family_index = u32::try_from(queue_family_index).map_err(|_| {
            VulkanOffscreenError::NoCompatibleDevice {
                detail: "queue-family index exceeds u32".to_owned(),
            }
        })?;
        return Ok(DeviceSelection {
            physical_device,
            queue_family_index,
            facts: VulkanDeviceFacts {
                device_name,
                vendor_id: properties.vendor_id,
                device_id: properties.device_id,
                driver_version: properties.driver_version,
                api_version: properties.api_version,
                queue_family_index,
                queue_flags: family.queue_flags,
                max_image_dimension_2d: properties.limits.max_image_dimension2_d,
                enabled_instance_extensions: Vec::new(),
                enabled_device_extensions: Vec::new(),
            },
        });
    }
    let detail = if rejection_reasons.is_empty() {
        "the Vulkan loader reported no physical devices".to_owned()
    } else {
        rejection_reasons.join("; ")
    };
    Err(VulkanOffscreenError::NoCompatibleDevice { detail })
}

fn device_name(properties: &vk::PhysicalDeviceProperties) -> String {
    // SAFETY: Vulkan guarantees a NUL-terminated device_name field.
    unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn checked_byte_len(width: u32, height: u32) -> Result<vk::DeviceSize, VulkanOffscreenError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| VulkanOffscreenError::InvalidExtent {
            width,
            height,
            detail: "RGBA8 byte length overflows Vulkan device size".to_owned(),
        })
}

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn allocation_error(
    shared: &DeviceShared,
    resource: &'static str,
    source: vk::Result,
) -> VulkanOffscreenError {
    if source == vk::Result::ERROR_DEVICE_LOST {
        shared.device_lost.store(true, Ordering::Release);
        VulkanOffscreenError::DeviceLost {
            operation: "resource allocation",
        }
    } else {
        VulkanOffscreenError::Allocation { resource, source }
    }
}

fn operation_error(
    shared: &DeviceShared,
    operation: &'static str,
    source: vk::Result,
) -> VulkanOffscreenError {
    classify_operation_error(&shared.device_lost, operation, source)
}

fn classify_operation_error(
    device_lost: &AtomicBool,
    operation: &'static str,
    source: vk::Result,
) -> VulkanOffscreenError {
    if source == vk::Result::ERROR_DEVICE_LOST {
        device_lost.store(true, Ordering::Release);
        VulkanOffscreenError::DeviceLost { operation }
    } else {
        VulkanOffscreenError::Operation { operation, source }
    }
}

fn poll_until_complete<F>(
    timeout: Duration,
    cancellation: Option<&VulkanOffscreenCancellation>,
    mut wait: F,
) -> Result<(), VulkanOffscreenError>
where
    F: FnMut(u64) -> Result<(), vk::Result>,
{
    let start = Instant::now();
    loop {
        if cancellation.is_some_and(VulkanOffscreenCancellation::is_cancelled) {
            return Err(VulkanOffscreenError::Cancelled);
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(VulkanOffscreenError::Timeout { timeout });
        }
        let remaining = timeout.saturating_sub(elapsed);
        let wait_for = remaining.min(FENCE_POLL_INTERVAL);
        let wait_ns = u64::try_from(wait_for.as_nanos()).unwrap_or(u64::MAX);
        match wait(wait_ns) {
            Ok(()) => return Ok(()),
            Err(vk::Result::TIMEOUT) => {}
            Err(source) => {
                return Err(VulkanOffscreenError::Operation {
                    operation: "fence wait",
                    source,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn missing_loader_is_typed_unavailable() {
        let result = VulkanOffscreenContext::with_entry_loader(2, || {
            Err(VulkanOffscreenError::LoaderUnavailable {
                detail: "synthetic missing vulkan-1.dll".to_owned(),
            })
        });
        let availability = VulkanOffscreenAvailability::from_result(result);
        let VulkanOffscreenAvailability::Unavailable(unavailable) = availability else {
            panic!("synthetic missing loader must be unavailable");
        };
        assert_eq!(unavailable.kind, VulkanOffscreenUnavailableKind::Loader);
        assert!(unavailable.message.contains("Vulkan loader unavailable"));
        assert!(unavailable.message.contains("vulkan-1.dll"));
    }

    #[test]
    fn cancellation_is_observed_during_fence_polling() {
        let cancellation = VulkanOffscreenCancellation::default();
        let polls = AtomicUsize::new(0);
        let result = poll_until_complete(Duration::from_secs(1), Some(&cancellation), |_| {
            polls.fetch_add(1, Ordering::Relaxed);
            cancellation.cancel();
            Err(vk::Result::TIMEOUT)
        });
        assert!(matches!(result, Err(VulkanOffscreenError::Cancelled)));
        assert_eq!(polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn device_loss_is_classified_and_sticky() {
        let lost = AtomicBool::new(false);
        let error =
            classify_operation_error(&lost, "synthetic submit", vk::Result::ERROR_DEVICE_LOST);
        assert!(matches!(
            error,
            VulkanOffscreenError::DeviceLost {
                operation: "synthetic submit"
            }
        ));
        assert!(lost.load(Ordering::Acquire));
    }

    #[test]
    fn zero_cache_capacity_is_rejected_before_loading_vulkan() {
        let loader_called = AtomicBool::new(false);
        let result = VulkanOffscreenContext::with_entry_loader(0, || {
            loader_called.store(true, Ordering::Release);
            Err(VulkanOffscreenError::LoaderUnavailable {
                detail: "must not run".to_owned(),
            })
        });
        assert!(matches!(
            result,
            Err(VulkanOffscreenError::InvalidCacheCapacity)
        ));
        assert!(!loader_called.load(Ordering::Acquire));
    }

    #[test]
    fn clear_readback_and_cache_reuse_when_vulkan_is_available() {
        let _gpu_test = gpu_test_lock();
        let context = match VulkanOffscreenContext::with_target_cache_capacity(2) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only clear/readback proof: {error}");
                return;
            }
        };
        eprintln!(
            "no-window Vulkan device facts: {:?}",
            context.device_facts()
        );
        assert!(
            context
                .device_facts()
                .enabled_instance_extensions
                .is_empty()
        );
        assert!(context.device_facts().enabled_device_extensions.is_empty());
        let session = context.create_session();
        let a1 = session.target(7, 5).expect("target A should allocate");
        assert!(a1.staging_capacity_bytes() >= 7 * 5 * 4);
        let readback = a1
            .clear_and_readback([1.0, 0.0, 0.0, 1.0], Duration::from_secs(5))
            .expect("clear/readback should complete");
        assert_eq!(readback.width, 7);
        assert_eq!(readback.height, 5);
        assert_eq!(readback.rgba, [255, 0, 0, 255].repeat(35));
        let a2 = session.target(7, 5).expect("same-size target should reuse");
        assert_eq!(a1.allocation_id(), a2.allocation_id());
        let b = session.target(9, 3).expect("target B should allocate");
        assert_ne!(a1.allocation_id(), b.allocation_id());
        let a3 = session.target(7, 5).expect("A should survive A-B-A");
        assert_eq!(a1.allocation_id(), a3.allocation_id());
        let stats = context.cache_stats();
        assert_eq!(stats.capacity, 2);
        assert_eq!(stats.retained_targets, 2);
        assert_eq!(stats.allocations, 2);
        assert!(stats.reuses >= 2);
    }

    #[test]
    fn crate_internal_compute_recording_seam_submits_when_vulkan_is_available() {
        let _gpu_test = gpu_test_lock();
        let context = match VulkanOffscreenContext::new() {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only compute seam proof: {error}");
                return;
            }
        };
        let access = context.device_access();
        assert_ne!(access.instance().handle(), vk::Instance::null());
        assert_ne!(access.physical_device(), vk::PhysicalDevice::null());
        assert_ne!(access.device().handle(), vk::Device::null());
        assert_eq!(
            access.queue_family_index(),
            context.device_facts().queue_family_index
        );
        assert_eq!(access.target_format(), OFFSCREEN_FORMAT);

        let target = context
            .create_session()
            .target(5, 3)
            .expect("compute target should allocate");
        let readback = target
            .record_compute_and_readback(Duration::from_secs(5), None, |recording| {
                assert_ne!(recording.device().handle(), vk::Device::null());
                assert_ne!(recording.command_buffer(), vk::CommandBuffer::null());
                assert_ne!(recording.image(), vk::Image::null());
                assert_eq!(
                    recording.extent(),
                    vk::Extent2D {
                        width: 5,
                        height: 3
                    }
                );
                assert_eq!(recording.format(), OFFSCREEN_FORMAT);
                // The later slug module records its bind/dispatch commands here.
                // An empty compute pass is sufficient to prove this runtime owns
                // layout transitions, submission, completion, and readback.
                Ok(())
            })
            .expect("empty compute recording seam should submit and read back");
        assert_eq!(readback.rgba.len(), 5 * 3 * 4);
    }

    #[test]
    fn regional_compute_readback_copies_only_tight_requested_pixels_when_vulkan_is_available() {
        let _gpu_test = gpu_test_lock();
        let context = match VulkanOffscreenContext::new() {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only regional readback proof: {error}");
                return;
            }
        };
        let target = context
            .create_session()
            .target(16, 12)
            .expect("regional target should allocate");
        let regions = [
            VulkanOffscreenRegion {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            VulkanOffscreenRegion {
                x: 11,
                y: 8,
                width: 5,
                height: 2,
            },
        ];
        target
            .clear_and_readback([0.25, 0.5, 0.75, 1.0], Duration::from_secs(5))
            .expect("setup clear should complete");
        let readback = target
            .record_compute_and_readback_regions(
                &regions,
                Duration::from_secs(5),
                None,
                |_recording| Ok(()),
            )
            .expect("regional readback should complete");
        assert_eq!((readback.frame_width, readback.frame_height), (16, 12));
        assert_eq!(readback.regions.len(), 2);
        assert_eq!(readback.regions[0].region, regions[0]);
        assert_eq!(readback.regions[1].region, regions[1]);
        assert_eq!(readback.regions[0].rgba, [64, 127, 191, 255].repeat(12));
        assert_eq!(readback.regions[1].rgba, [64, 127, 191, 255].repeat(10));
        let copied_bytes = readback
            .regions
            .iter()
            .map(|region| region.rgba.len())
            .sum::<usize>();
        assert_eq!(copied_bytes, (3 * 4 + 5 * 2) * 4);
        assert!(copied_bytes < 16 * 12 * 4);
    }

    #[test]
    fn bounded_eviction_and_independent_sessions_when_vulkan_is_available() {
        let _gpu_test = gpu_test_lock();
        let context = match VulkanOffscreenContext::with_target_cache_capacity(2) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only cache proof: {error}");
                return;
            }
        };
        let left = context.create_session();
        let right = context.create_session();
        let left_target = left.target(4, 4).expect("left target should allocate");
        let right_target = right.target(4, 4).expect("right target should allocate");
        assert_ne!(left_target.allocation_id(), right_target.allocation_id());
        let left_pixels = left_target
            .clear_and_readback([0.0, 1.0, 0.0, 1.0], Duration::from_secs(5))
            .expect("left readback should complete");
        let right_pixels = right_target
            .clear_and_readback([0.0, 0.0, 1.0, 1.0], Duration::from_secs(5))
            .expect("right readback should complete");
        assert_eq!(left_pixels.rgba, [0, 255, 0, 255].repeat(16));
        assert_eq!(right_pixels.rgba, [0, 0, 255, 255].repeat(16));

        let third = left.target(8, 2).expect("third target should allocate");
        assert_ne!(third.allocation_id(), left_target.allocation_id());
        let stats = context.cache_stats();
        assert_eq!(stats.retained_targets, 2);
        assert_eq!(stats.evictions, 1);

        drop(left_target);
        drop(right_target);
        drop(third);
        left.clear_cached_targets();
        right.clear_cached_targets();
        assert_eq!(context.cache_stats().retained_targets, 0);
        assert_eq!(
            context.cache_stats().allocations,
            context.cache_stats().destructions
        );
    }

    #[cfg(windows)]
    #[test]
    fn offscreen_context_creates_no_top_level_windows() {
        let _gpu_test = gpu_test_lock();
        let before = current_process_top_level_windows();
        let context = match VulkanOffscreenContext::new() {
            Ok(context) => context,
            Err(error) => {
                eprintln!("SKIP GPU-only no-HWND proof: {error}");
                return;
            }
        };
        let target = context
            .create_session()
            .target(3, 2)
            .expect("offscreen target should allocate");
        target
            .clear_and_readback([1.0, 1.0, 1.0, 1.0], Duration::from_secs(5))
            .expect("offscreen proof should render");
        let after = current_process_top_level_windows();
        let created = after.difference(&before).copied().collect::<Vec<_>>();
        assert!(
            created.is_empty(),
            "offscreen Vulkan created HWNDs: {created:?}"
        );
    }

    #[cfg(windows)]
    fn current_process_top_level_windows() -> std::collections::HashSet<isize> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Foundation::LPARAM;
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        use windows::core::BOOL;

        struct Enumeration {
            process_id: u32,
            windows: std::collections::HashSet<isize>,
        }

        unsafe extern "system" fn callback(window: HWND, state: LPARAM) -> BOOL {
            // SAFETY: EnumWindows passes back the non-null Enumeration pointer
            // supplied below for the synchronous duration of the call.
            let state = unsafe { &mut *(state.0 as *mut Enumeration) };
            let mut owner = 0_u32;
            // SAFETY: The HWND comes from EnumWindows and owner points to live stack storage.
            unsafe { GetWindowThreadProcessId(window, Some(&mut owner)) };
            if owner == state.process_id {
                state.windows.insert(window.0 as isize);
            }
            true.into()
        }

        let mut enumeration = Enumeration {
            // SAFETY: This function takes no pointers and returns the current PID.
            process_id: unsafe { GetCurrentProcessId() },
            windows: std::collections::HashSet::new(),
        };
        // SAFETY: The callback and LPARAM remain valid for this synchronous enumeration.
        unsafe {
            EnumWindows(
                Some(callback),
                LPARAM((&raw mut enumeration).cast::<std::ffi::c_void>() as isize),
            )
        }
        .expect("EnumWindows should enumerate the current desktop");
        enumeration.windows
    }

    fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
