//! Minimal Ash/Vulkan presentation for terminal RGBA frames.
//!
//! The font crate rasterizes text into a host-side RGBA frame. This crate
//! still owns the complete window, device, swapchain, synchronization, and
//! presentation path: the frame is uploaded through a Vulkan staging buffer
//! and copied into a Vulkan swapchain image. There is intentionally no CPU
//! presentation fallback.

use ash::Entry;
use ash::Instance;
use ash::vk;
use eyre::Context;
use eyre::Result;
use raw_window_handle::HasDisplayHandle;
use raw_window_handle::HasWindowHandle;
use std::ffi::CString;
use teamy_terminal_font::RasterizedFrame;
use winit::window::Window;

struct SwapchainState {
    handle: vk::SwapchainKHR,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    image_revisions: Vec<Option<u64>>,
    swap_red_blue: bool,
}

/// A rectangular range of RGBA pixels to upload and present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    /// Zero-based left coordinate.
    pub x: u32,
    /// Zero-based top coordinate.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl PixelRect {
    /// Construct a pixel rectangle.
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Cover an entire frame.
    #[must_use]
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }
}

struct UploadRegion {
    rect: PixelRect,
    buffer_offset: vk::DeviceSize,
}

struct StagingBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
}

/// A one-frame-in-flight Vulkan presenter for terminal frames.
pub struct VulkanRenderer {
    _entry: Entry,
    instance: Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical_device: vk::PhysicalDevice,
    device: ash::Device,
    swapchain_loader: ash::khr::swapchain::Device,
    queue_family_index: u32,
    queue: vk::Queue,
    swapchain: SwapchainState,
    image_available: vk::Semaphore,
    render_finished: Vec<vk::Semaphore>,
    fence: vk::Fence,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    staging: Option<StagingBuffer>,
    current_frame_revision: u64,
    last_upload_bytes: usize,
    last_upload_full_refresh: bool,
    last_upload_region_count: usize,
}

impl std::fmt::Debug for VulkanRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanRenderer")
            .field("physical_device", &self.physical_device)
            .field("queue_family_index", &self.queue_family_index)
            .field("extent", &self.swapchain.extent)
            .field("image_count", &self.swapchain.images.len())
            .finish_non_exhaustive()
    }
}

impl VulkanRenderer {
    /// Construct a Vulkan presenter for a winit window.
    ///
    /// Construction errors are intentionally fatal to the interactive
    /// application. The caller should surface the returned diagnostic and
    /// exit rather than silently selecting a different rendering path.
    ///
    /// # Errors
    ///
    /// Returns an actionable error when Vulkan, the window surface, the
    /// selected device, or the initial swapchain cannot be constructed.
    ///
    /// # Panics
    ///
    /// This function does not panic for runtime Vulkan failures. The static
    /// application and engine names are known to contain no NUL bytes.
    pub fn new(window: &Window) -> Result<Self> {
        // SAFETY: Loading the process Vulkan entry points does not access any
        // application-owned memory and the returned entry is kept alive.
        let entry = unsafe { Entry::load() }
            .wrap_err("failed to load the Vulkan loader; install a Vulkan runtime/driver")?;
        let app_name =
            CString::new("teamy-terminal").wrap_err("invalid Vulkan application name")?;
        let engine_name = CString::new("teamy-terminal").wrap_err("invalid Vulkan engine name")?;
        let display_handle = window
            .display_handle()
            .wrap_err("winit did not provide a display handle for Vulkan")?
            .as_raw();
        let extension_names = ash_window::enumerate_required_extensions(display_handle)
            .wrap_err("the window platform did not expose required Vulkan extensions")?;
        let application_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(1)
            .engine_name(&engine_name)
            .engine_version(1)
            .api_version(vk::API_VERSION_1_0);
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&application_info)
            .enabled_extension_names(extension_names);
        // SAFETY: The create-info references local values that remain alive
        // for the duration of the call and the requested extensions came from
        // the active window platform.
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .wrap_err("failed to create the Vulkan instance")?;

        let window_handle = window
            .window_handle()
            .wrap_err("winit did not provide a window handle for Vulkan")?
            .as_raw();
        // SAFETY: The raw handles are supplied by winit and remain valid while
        // the window is owned by the interactive application.
        let surface = unsafe {
            ash_window::create_surface(&entry, &instance, display_handle, window_handle, None)
        }
        .wrap_err("failed to create the Vulkan presentation surface")?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);

        let (physical_device, queue_family_index) =
            pick_physical_device(&instance, &surface_loader, surface)?;
        let queue_priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities)];
        let device_extensions = [ash::khr::swapchain::NAME.as_ptr()];
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions);
        // SAFETY: The selected queue family and swapchain extension were
        // checked against the physical device before device creation.
        let device = unsafe { instance.create_device(physical_device, &device_info, None) }
            .wrap_err("failed to create a Vulkan logical device")?;
        // SAFETY: Queue zero exists because the queue-create-info requested it.
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let swapchain = create_swapchain(
            window,
            physical_device,
            &surface_loader,
            surface,
            &swapchain_loader,
            queue_family_index,
            vk::SwapchainKHR::null(),
        )?;

        let semaphore_info = vk::SemaphoreCreateInfo::default();
        // SAFETY: The device is valid and the semaphore create-info is fully
        // initialized.
        let image_available = unsafe { device.create_semaphore(&semaphore_info, None) }
            .wrap_err("failed to create Vulkan image-available semaphore")?;
        let render_finished = create_render_finished_semaphores(&device, swapchain.images.len())?;
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: The device is valid and the fence create-info is fully
        // initialized.
        let fence = unsafe { device.create_fence(&fence_info, None) }
            .wrap_err("failed to create Vulkan frame fence")?;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: The selected queue family is a valid graphics/present
        // family on this device.
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }
            .wrap_err("failed to create Vulkan command pool")?;
        let command_buffer_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: The command pool and allocation request are valid.
        let command_buffer = unsafe { device.allocate_command_buffers(&command_buffer_info) }
            .wrap_err("failed to allocate Vulkan command buffer")?
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("Vulkan returned no command buffer"))?;

        Ok(Self {
            _entry: entry,
            instance,
            surface_loader,
            surface,
            physical_device,
            device,
            swapchain_loader,
            queue_family_index,
            queue,
            swapchain,
            image_available,
            render_finished,
            fence,
            command_pool,
            command_buffer,
            staging: None,
            current_frame_revision: 0,
            last_upload_bytes: 0,
            last_upload_full_refresh: false,
            last_upload_region_count: 0,
        })
    }

    /// Current swapchain extent in physical pixels.
    #[must_use = "the extent is needed to size the next terminal frame"]
    pub const fn extent(&self) -> vk::Extent2D {
        self.swapchain.extent
    }

    /// Number of bytes uploaded by the most recent successful render call.
    #[must_use]
    pub const fn last_upload_bytes(&self) -> usize {
        self.last_upload_bytes
    }

    /// Whether the most recent successful render copied a complete frame.
    #[must_use]
    pub const fn last_upload_was_full_refresh(&self) -> bool {
        self.last_upload_full_refresh
    }

    /// Number of buffer-to-image regions submitted by the most recent
    /// successful render.
    #[must_use]
    pub const fn last_upload_region_count(&self) -> usize {
        self.last_upload_region_count
    }

    /// Upload and present one RGBA frame. Returns `true` when the swapchain
    /// should be recreated before the next frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the frame size, staging upload, command
    /// recording, queue submission, or presentation fails.
    pub fn render(&mut self, frame: &RasterizedFrame) -> Result<bool> {
        let full = [PixelRect::full(frame.width, frame.height)];
        self.render_regions(frame, &full, true)
    }

    /// Upload and present only the changed pixel rectangles in a cached frame.
    ///
    /// Each swapchain image is tracked independently. The first time an image
    /// is acquired after creation or recreation, the complete frame is copied;
    /// later acquisitions may copy only `regions` because the image already
    /// contains the previous complete frame.
    ///
    /// # Errors
    ///
    /// Returns an error when a region is invalid for the frame, staging upload,
    /// command recording, queue submission, or presentation fails.
    pub fn render_damage(
        &mut self,
        frame: &RasterizedFrame,
        regions: &[PixelRect],
    ) -> Result<bool> {
        self.render_regions(frame, regions, false)
    }

    fn render_regions(
        &mut self,
        frame: &RasterizedFrame,
        requested_regions: &[PixelRect],
        force_full: bool,
    ) -> Result<bool> {
        if frame.width != self.swapchain.extent.width
            || frame.height != self.swapchain.extent.height
        {
            return Err(eyre::eyre!(
                "frame extent {}x{} does not match Vulkan swapchain {}x{}",
                frame.width,
                frame.height,
                self.swapchain.extent.width,
                self.swapchain.extent.height
            ));
        }
        if frame.pixels.len()
            != usize::try_from(frame.width)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(frame.height).unwrap_or(0))
                .saturating_mul(4)
        {
            return Err(eyre::eyre!(
                "terminal frame pixel length does not match its extent"
            ));
        }
        if !force_full && requested_regions.is_empty() {
            return Ok(false);
        }
        self.wait_for_frame()?;
        let (image_index, suboptimal) = self.acquire_image()?;
        if image_index == u32::MAX {
            return Ok(true);
        }
        let image_index_usize = usize::try_from(image_index).expect("image index fits");
        let current_revision = self.current_frame_revision;
        let next_revision = current_revision.wrapping_add(1);
        let full_refresh = force_full
            || self
                .swapchain
                .image_revisions
                .get(image_index_usize)
                .copied()
                .flatten()
                != Some(current_revision);
        let regions = if full_refresh {
            vec![PixelRect::full(frame.width, frame.height)]
        } else {
            requested_regions.to_vec()
        };
        for region in &regions {
            validate_pixel_rect(*region, frame.width, frame.height)?;
        }

        let mut transformed = None;
        let mut packed = None;
        let upload_regions = if full_refresh {
            if self.swapchain.swap_red_blue {
                let mut pixels = frame.pixels.clone();
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                transformed = Some(pixels);
            }
            vec![UploadRegion {
                rect: PixelRect::full(frame.width, frame.height),
                buffer_offset: 0,
            }]
        } else {
            let (pixels, upload_regions) =
                pack_regions(frame, &regions, self.swapchain.swap_red_blue)?;
            packed = Some(pixels);
            upload_regions
        };
        let upload_bytes: &[u8] = if let Some(pixels) = transformed.as_deref() {
            pixels
        } else if let Some(pixels) = packed.as_deref() {
            pixels
        } else {
            &frame.pixels
        };
        let byte_count = vk::DeviceSize::try_from(upload_bytes.len())
            .wrap_err("terminal frame is too large for a Vulkan staging buffer")?;
        self.ensure_staging_buffer(byte_count)?;
        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| eyre::eyre!("Vulkan staging buffer was not created"))?;
        self.upload_frame(staging.memory, byte_count, upload_bytes)?;
        let upload_byte_count = upload_bytes.len();
        self.record_copy_command(image_index, staging.buffer, &upload_regions, full_refresh)?;
        let result = self.submit_and_present(image_index, suboptimal);
        if result.is_ok()
            && let Some(revision) = self.swapchain.image_revisions.get_mut(image_index_usize)
        {
            *revision = Some(next_revision);
            self.current_frame_revision = next_revision;
            self.last_upload_bytes = upload_byte_count;
            self.last_upload_full_refresh = full_refresh;
            self.last_upload_region_count = upload_regions.len();
        }
        result
    }

    /// Recreate the swapchain after a resize or an out-of-date presentation.
    ///
    /// # Errors
    ///
    /// Returns an error when the device cannot idle or the replacement
    /// swapchain cannot be constructed.
    pub fn recreate(&mut self, window: &Window) -> Result<()> {
        // SAFETY: No work may reference the swapchain after the device has
        // been idled, so destroying this handle is safe.
        unsafe {
            self.device
                .device_wait_idle()
                .wrap_err("failed waiting for Vulkan before swapchain recreation")?;
        };
        let new_swapchain = create_swapchain(
            window,
            self.physical_device,
            &self.surface_loader,
            self.surface,
            &self.swapchain_loader,
            self.queue_family_index,
            self.swapchain.handle,
        )?;
        let new_render_finished =
            match create_render_finished_semaphores(&self.device, new_swapchain.images.len()) {
                Ok(semaphores) => semaphores,
                Err(error) => {
                    // SAFETY: The new swapchain was created by this device and no
                    // work has been submitted against it yet.
                    unsafe {
                        self.swapchain_loader
                            .destroy_swapchain(new_swapchain.handle, None);
                    };
                    return Err(error);
                }
            };
        let old_swapchain = self.swapchain.handle;
        let old_render_finished = std::mem::replace(&mut self.render_finished, new_render_finished);
        for semaphore in old_render_finished {
            // SAFETY: The device is idle, so no present operation can still
            // reference the old image-indexed semaphore.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        // SAFETY: The device is idle and the old swapchain handle is owned by
        // this renderer.
        unsafe {
            self.swapchain_loader.destroy_swapchain(old_swapchain, None);
        };
        self.swapchain = new_swapchain;
        Ok(())
    }

    fn wait_for_frame(&self) -> Result<()> {
        // SAFETY: The fence belongs to this renderer and is not destroyed
        // until after the device has been idled.
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .wrap_err("failed waiting for the Vulkan frame fence")
        }
    }

    fn upload_frame(
        &self,
        memory: vk::DeviceMemory,
        byte_count: vk::DeviceSize,
        bytes: &[u8],
    ) -> Result<()> {
        // SAFETY: The staging allocation is host-visible, the byte count is
        // within the allocation, and the mapped pointer is used only for the
        // duration of this upload.
        let mapped = unsafe {
            self.device
                .map_memory(memory, 0, byte_count, vk::MemoryMapFlags::empty())
                .wrap_err("failed to map Vulkan staging memory")?
        };
        // SAFETY: `mapped` points to an allocation at least `bytes.len()` bytes
        // long and the source slice is valid for that many bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len()) };
        // SAFETY: The memory was mapped by this renderer and is not used by a
        // command buffer until after this function returns.
        unsafe { self.device.unmap_memory(memory) };
        Ok(())
    }

    fn acquire_image(&self) -> Result<(u32, bool)> {
        // SAFETY: The swapchain and semaphore are owned by this live renderer.
        let acquired = unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain.handle,
                u64::MAX,
                self.image_available,
                vk::Fence::null(),
            )
        };
        match acquired {
            Ok(value) => Ok(value),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok((u32::MAX, true)),
            Err(error) => Err(error).wrap_err("failed to acquire a Vulkan swapchain image"),
        }
    }

    fn record_copy_command(
        &self,
        image_index: u32,
        staging_buffer: vk::Buffer,
        regions: &[UploadRegion],
        full_refresh: bool,
    ) -> Result<()> {
        if image_index == u32::MAX {
            return Ok(());
        }
        // SAFETY: The command buffer is not pending after the frame fence was
        // waited and its pool was created with RESET_COMMAND_BUFFER.
        unsafe {
            self.device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
                .wrap_err("failed to reset the Vulkan command buffer")?;
        };
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: The command buffer is in the initial state after reset and
        // the begin-info is fully initialized.
        unsafe {
            self.device
                .begin_command_buffer(self.command_buffer, &begin_info)
                .wrap_err("failed to begin the Vulkan command buffer")?;
        };

        let image = self.swapchain.images[usize::try_from(image_index).expect("image index fits")];
        let range = color_subresource_range();
        let image_initialized = self.swapchain.image_revisions
            [usize::try_from(image_index).expect("image index fits")]
        .is_some();
        let (old_layout, source_stage, source_access) = if image_initialized {
            (
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags::empty(),
            )
        } else {
            (
                vk::ImageLayout::UNDEFINED,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::AccessFlags::empty(),
            )
        };
        let to_transfer = vk::ImageMemoryBarrier::default()
            .src_access_mask(source_access)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range);
        // SAFETY: The command buffer is recording and the image barrier
        // describes the selected colour swapchain image.
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                source_stage,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_transfer),
            );
        };
        let copies = regions
            .iter()
            .map(|region| {
                let rect = region.rect;
                let (buffer_row_length, buffer_image_height) = if full_refresh {
                    (0, 0)
                } else {
                    (rect.width, rect.height)
                };
                Ok(vk::BufferImageCopy::default()
                    .buffer_offset(region.buffer_offset)
                    .buffer_row_length(buffer_row_length)
                    .buffer_image_height(buffer_image_height)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_offset(vk::Offset3D {
                        x: i32::try_from(rect.x)
                            .wrap_err("dirty rectangle x exceeds Vulkan limits")?,
                        y: i32::try_from(rect.y)
                            .wrap_err("dirty rectangle y exceeds Vulkan limits")?,
                        z: 0,
                    })
                    .image_extent(vk::Extent3D {
                        width: rect.width,
                        height: rect.height,
                        depth: 1,
                    }))
            })
            .collect::<Result<Vec<_>>>()?;
        // SAFETY: The staging buffer and image are valid transfer resources,
        // and the image is in TRANSFER_DST_OPTIMAL after the preceding barrier.
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                self.command_buffer,
                staging_buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &copies,
            );
        };
        let to_present = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range);
        // SAFETY: The copy command precedes this barrier in the same command
        // buffer, so the image can transition to the presentation layout.
        unsafe {
            self.device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_present),
            );
        };
        // SAFETY: All commands have been recorded into this command buffer.
        unsafe {
            self.device
                .end_command_buffer(self.command_buffer)
                .wrap_err("failed to end the Vulkan command buffer")?;
        };
        Ok(())
    }

    fn submit_and_present(&self, image_index: u32, suboptimal: bool) -> Result<bool> {
        if image_index == u32::MAX {
            return Ok(true);
        }
        let wait_semaphores = [self.image_available];
        let wait_stages = [vk::PipelineStageFlags::TRANSFER];
        let signal_semaphores =
            [self.render_finished[usize::try_from(image_index).expect("image index fits")]];
        let command_buffers = [self.command_buffer];
        let submit_info = [vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&command_buffers)
            .signal_semaphores(&signal_semaphores)];
        // SAFETY: The frame fence was waited before recording this command
        // buffer, and remains signaled until it is reset immediately before
        // submission.
        unsafe {
            self.device
                .reset_fences(&[self.fence])
                .wrap_err("failed to reset the Vulkan frame fence")?;
        };
        // SAFETY: The wait/signal semaphores, command buffer, queue, and fence
        // are all owned by this renderer and are in the states established by
        // the preceding acquire/record operations.
        unsafe {
            self.device
                .queue_submit(self.queue, &submit_info, self.fence)
                .wrap_err("failed to submit the terminal frame to Vulkan")?;
        };
        let swapchains = [self.swapchain.handle];
        let image_indices = [image_index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        // SAFETY: The present info references the live swapchain and the
        // semaphore signalled by the just-submitted command buffer.
        let present = unsafe {
            self.swapchain_loader
                .queue_present(self.queue, &present_info)
        };
        match present {
            Ok(present_suboptimal) => Ok(suboptimal || present_suboptimal),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::SUBOPTIMAL_KHR) => Ok(true),
            Err(error) => Err(error).wrap_err("failed to present the terminal frame"),
        }
    }

    fn ensure_staging_buffer(&mut self, required: vk::DeviceSize) -> Result<()> {
        if self
            .staging
            .as_ref()
            .is_some_and(|buffer| buffer.capacity >= required)
        {
            return Ok(());
        }
        if let Some(old) = self.staging.take() {
            // SAFETY: The previous frame fence was waited before resizing the
            // staging allocation, so no command references this buffer.
            unsafe { self.device.destroy_buffer(old.buffer, None) };
            // SAFETY: The previous buffer has been destroyed and its memory is
            // no longer referenced by Vulkan.
            unsafe { self.device.free_memory(old.memory, None) };
        }
        let info = vk::BufferCreateInfo::default()
            .size(required.max(4))
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: The buffer create-info is fully initialized and the device
        // is live.
        let buffer = unsafe { self.device.create_buffer(&info, None) }
            .wrap_err("failed to create Vulkan terminal staging buffer")?;
        // SAFETY: `buffer` was successfully created on this device.
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            &self.instance,
            self.physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let allocation = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: The allocation size and memory type came from this device's
        // buffer requirements and memory properties.
        let memory = unsafe { self.device.allocate_memory(&allocation, None) }
            .wrap_err("failed to allocate Vulkan terminal staging memory")?;
        // SAFETY: The allocation and buffer belong to the same device and the
        // offset is aligned by Vulkan's allocation requirements.
        unsafe {
            self.device
                .bind_buffer_memory(buffer, memory, 0)
                .wrap_err("failed to bind Vulkan terminal staging memory")?;
        };
        self.staging = Some(StagingBuffer {
            buffer,
            memory,
            capacity: required.max(4),
        });
        Ok(())
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        // SAFETY: Drop is the final owner of the device and waits for all
        // submitted work before destroying dependent resources.
        unsafe {
            let _ = self.device.device_wait_idle();
        }
        if let Some(staging) = self.staging.take() {
            // SAFETY: The device is idle and no command references staging.
            unsafe { self.device.destroy_buffer(staging.buffer, None) };
            // SAFETY: The staging buffer has been destroyed first.
            unsafe { self.device.free_memory(staging.memory, None) };
        }
        // SAFETY: The device is idle and owns this command pool.
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
        // SAFETY: The device is idle and owns this fence.
        unsafe { self.device.destroy_fence(self.fence, None) };
        // SAFETY: The device is idle and owns this semaphore.
        unsafe { self.device.destroy_semaphore(self.image_available, None) };
        for semaphore in self.render_finished.drain(..) {
            // SAFETY: The device is idle and owns this semaphore.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        // SAFETY: The device is idle and owns this swapchain.
        unsafe {
            self.swapchain_loader
                .destroy_swapchain(self.swapchain.handle, None);
        };
        // SAFETY: All device-owned resources have been destroyed.
        unsafe { self.device.destroy_device(None) };
        // SAFETY: The surface is no longer used by the destroyed device.
        unsafe { self.surface_loader.destroy_surface(self.surface, None) };
        // SAFETY: The instance is the final Vulkan object being destroyed.
        unsafe { self.instance.destroy_instance(None) };
    }
}

fn validate_pixel_rect(rect: PixelRect, frame_width: u32, frame_height: u32) -> Result<()> {
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or_else(|| eyre::eyre!("dirty rectangle x extent overflows"))?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or_else(|| eyre::eyre!("dirty rectangle y extent overflows"))?;
    if rect.width == 0 || rect.height == 0 || right > frame_width || bottom > frame_height {
        return Err(eyre::eyre!(
            "dirty rectangle {}x{}+{}+{} is outside frame {}x{}",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            frame_width,
            frame_height
        ));
    }
    Ok(())
}

fn pack_regions(
    frame: &RasterizedFrame,
    regions: &[PixelRect],
    swap_red_blue: bool,
) -> Result<(Vec<u8>, Vec<UploadRegion>)> {
    let mut pixels = Vec::new();
    let mut upload_regions = Vec::with_capacity(regions.len());
    for rect in regions.iter().copied() {
        validate_pixel_rect(rect, frame.width, frame.height)?;
        let buffer_offset = vk::DeviceSize::try_from(pixels.len())
            .wrap_err("dirty rectangle staging offset exceeds Vulkan limits")?;
        for row in rect.y..rect.y + rect.height {
            let start = (usize::try_from(row).unwrap_or(0)
                * usize::try_from(frame.width).unwrap_or(0)
                + usize::try_from(rect.x).unwrap_or(0))
                * 4;
            let end = start + usize::try_from(rect.width).unwrap_or(0) * 4;
            let source = frame
                .pixels
                .get(start..end)
                .ok_or_else(|| eyre::eyre!("dirty rectangle row exceeds frame pixels"))?;
            if swap_red_blue {
                for pixel in source.chunks_exact(4) {
                    pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
            } else {
                pixels.extend_from_slice(source);
            }
        }
        upload_regions.push(UploadRegion {
            rect,
            buffer_offset,
        });
    }
    Ok((pixels, upload_regions))
}

fn create_render_finished_semaphores(
    device: &ash::Device,
    image_count: usize,
) -> Result<Vec<vk::Semaphore>> {
    let info = vk::SemaphoreCreateInfo::default();
    let mut semaphores = Vec::with_capacity(image_count);
    for _ in 0..image_count {
        // SAFETY: The device is live and the semaphore create-info is fully
        // initialized.
        match unsafe { device.create_semaphore(&info, None) } {
            Ok(semaphore) => semaphores.push(semaphore),
            Err(error) => {
                for semaphore in semaphores {
                    // SAFETY: The partially-created semaphore belongs to this
                    // live device and has not been submitted.
                    unsafe { device.destroy_semaphore(semaphore, None) };
                }
                return Err(error).wrap_err("failed to create Vulkan render-finished semaphore");
            }
        }
    }
    Ok(semaphores)
}

fn pick_physical_device(
    instance: &Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    // SAFETY: The Vulkan instance is live for the duration of device probing.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .wrap_err("failed to enumerate Vulkan physical devices")?;
    for physical_device in devices {
        // SAFETY: The physical device handle came from this instance.
        let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }
            .wrap_err("failed to enumerate Vulkan device extensions")?;
        let has_swapchain = extensions.iter().any(|extension| {
            // SAFETY: Vulkan guarantees a NUL-terminated extension name in
            // this fixed-size field.
            unsafe {
                std::ffi::CStr::from_ptr(extension.extension_name.as_ptr())
                    == ash::khr::swapchain::NAME
            }
        });
        if !has_swapchain {
            continue;
        }
        // SAFETY: The physical device handle came from this instance.
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        for (index, family) in families.iter().enumerate() {
            let graphics = family.queue_flags.contains(vk::QueueFlags::GRAPHICS);
            let queue_index = u32::try_from(index).expect("Vulkan queue-family index fits in u32");
            // SAFETY: The physical device and surface were created by this
            // instance and remain live during probing.
            let present = unsafe {
                surface_loader.get_physical_device_surface_support(
                    physical_device,
                    queue_index,
                    surface,
                )
            }
            .unwrap_or(false);
            if graphics && present {
                // SAFETY: The physical device and surface are valid handles
                // owned by this renderer construction path.
                let capabilities = unsafe {
                    surface_loader
                        .get_physical_device_surface_capabilities(physical_device, surface)
                }
                .wrap_err("failed to query Vulkan surface capabilities")?;
                if capabilities
                    .supported_usage_flags
                    .contains(vk::ImageUsageFlags::TRANSFER_DST)
                {
                    return Ok((physical_device, queue_index));
                }
            }
        }
    }
    Err(eyre::eyre!(
        "no Vulkan device with graphics, present, swapchain, and transfer-destination support was found"
    ))
}

fn create_swapchain(
    window: &Window,
    physical_device: vk::PhysicalDevice,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    swapchain_loader: &ash::khr::swapchain::Device,
    queue_family_index: u32,
    old_swapchain: vk::SwapchainKHR,
) -> Result<SwapchainState> {
    // SAFETY: The physical device and surface are live handles from the same
    // Vulkan instance.
    let capabilities = unsafe {
        surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
    }
    .wrap_err("failed to query Vulkan surface capabilities")?;
    // SAFETY: The physical device and surface are live handles from the same
    // Vulkan instance.
    let formats =
        unsafe { surface_loader.get_physical_device_surface_formats(physical_device, surface) }
            .wrap_err("failed to query Vulkan surface formats")?;
    if formats.is_empty() {
        return Err(eyre::eyre!(
            "the Vulkan surface reported no usable image formats"
        ));
    }
    let format = formats
        .iter()
        .copied()
        .find(|format| {
            format.format == vk::Format::R8G8B8A8_SRGB
                && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| {
            formats.iter().copied().find(|format| {
                format.format == vk::Format::B8G8R8A8_SRGB
                    && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
        })
        .or_else(|| {
            formats.iter().copied().find(|format| {
                matches!(
                    format.format,
                    vk::Format::R8G8B8A8_UNORM | vk::Format::B8G8R8A8_UNORM
                )
            })
        })
        .ok_or_else(|| eyre::eyre!("the Vulkan surface has no supported RGBA8/BGRA8 format"))?;
    let swap_red_blue = matches!(
        format.format,
        vk::Format::B8G8R8A8_SRGB | vk::Format::B8G8R8A8_UNORM
    );
    let extent = choose_extent(window, capabilities);
    // SAFETY: The physical device and surface are live handles from the same
    // Vulkan instance.
    let present_modes = unsafe {
        surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
    }
    .wrap_err("failed to query Vulkan present modes")?;
    let present_mode = if present_modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        vk::PresentModeKHR::FIFO
    };
    let mut image_count = capabilities.min_image_count.saturating_add(1);
    if capabilities.max_image_count != 0 {
        image_count = image_count.min(capabilities.max_image_count);
    }
    let composite_alpha = [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ]
    .into_iter()
    .find(|flag| capabilities.supported_composite_alpha.contains(*flag))
    .ok_or_else(|| eyre::eyre!("the Vulkan surface has no supported composite-alpha mode"))?;
    let info = vk::SwapchainCreateInfoKHR::default()
        .surface(surface)
        .min_image_count(image_count)
        .image_format(format.format)
        .image_color_space(format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .queue_family_indices(std::slice::from_ref(&queue_family_index))
        .pre_transform(capabilities.current_transform)
        .composite_alpha(composite_alpha)
        .present_mode(present_mode)
        .old_swapchain(old_swapchain)
        .clipped(false);
    // SAFETY: The create-info references a live surface and supported device
    // capabilities.
    let handle = match unsafe { swapchain_loader.create_swapchain(&info, None) } {
        Ok(handle) => handle,
        Err(error) => {
            return Err(eyre::eyre!(
                "failed to create the Vulkan swapchain: {error:?}; format={:?}, color_space={:?}, extent={}x{}, image_count={}, present_mode={:?}",
                format.format,
                format.color_space,
                extent.width,
                extent.height,
                image_count,
                present_mode
            ));
        }
    };
    // SAFETY: The swapchain was successfully created by this loader.
    let images = unsafe { swapchain_loader.get_swapchain_images(handle) }
        .wrap_err("failed to get Vulkan swapchain images")?;
    let image_revisions = vec![None; images.len()];
    Ok(SwapchainState {
        handle,
        extent,
        images,
        image_revisions,
        swap_red_blue,
    })
}

fn choose_extent(window: &Window, capabilities: vk::SurfaceCapabilitiesKHR) -> vk::Extent2D {
    if capabilities.current_extent.width != u32::MAX {
        return capabilities.current_extent;
    }
    let size = window.inner_size();
    vk::Extent2D {
        width: size.width.clamp(
            capabilities.min_image_extent.width,
            capabilities.max_image_extent.width,
        ),
        height: size.height.clamp(
            capabilities.min_image_extent.height,
            capabilities.max_image_extent.height,
        ),
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn find_memory_type(
    instance: &Instance,
    physical_device: vk::PhysicalDevice,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Result<u32> {
    // SAFETY: The physical device handle came from this instance.
    let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    for (index, memory_type) in properties.memory_types.iter().enumerate() {
        if (type_bits & (1 << index)) != 0 && memory_type.property_flags.contains(required) {
            return Ok(u32::try_from(index).expect("Vulkan memory type index fits in u32"));
        }
    }
    Err(eyre::eyre!(
        "no host-visible coherent Vulkan memory type is available for terminal frames"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swapchain_state_tracks_image_initialization() {
        let state = SwapchainState {
            handle: vk::SwapchainKHR::null(),
            extent: vk::Extent2D {
                width: 640,
                height: 480,
            },
            images: Vec::new(),
            image_revisions: Vec::new(),
            swap_red_blue: false,
        };
        assert_eq!(state.extent.width, 640);
        assert_eq!(state.extent.height, 480);
        assert!(state.image_revisions.is_empty());
    }

    #[test]
    fn packed_dirty_regions_use_tight_rows() {
        let frame = RasterizedFrame {
            width: 4,
            height: 2,
            pixels: (0_u8..32).collect(),
        };
        let (pixels, regions) = pack_regions(&frame, &[PixelRect::new(1, 0, 2, 2)], false)
            .expect("the rectangle should fit the frame");

        assert_eq!(
            pixels,
            vec![4, 5, 6, 7, 8, 9, 10, 11, 20, 21, 22, 23, 24, 25, 26, 27]
        );
        assert_eq!(regions[0].buffer_offset, 0);
        assert!(validate_pixel_rect(PixelRect::new(3, 1, 2, 1), 4, 2).is_err());
    }
}
