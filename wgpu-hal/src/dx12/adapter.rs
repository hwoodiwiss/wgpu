use super::{conv, HResult as _};
use std::{mem, sync::Arc, thread};
use windows::Win32::{
    Foundation,
    Graphics::{Direct3D12, Dxgi},
    UI::WindowsAndMessaging::GetClientRect,
};

impl Drop for super::Adapter {
    fn drop(&mut self) {
        // Debug tracking alive objects
        if !thread::panicking()
            && self
                .private_caps
                .instance_flags
                .contains(crate::InstanceFlags::VALIDATION)
        {
            unsafe {
                self.report_live_objects();
            }
        }
        unsafe {
            self.raw.destroy();
        }
    }
}

impl super::Adapter {
    pub unsafe fn report_live_objects(&self) {
        if let Ok(debug_device) = self.raw.cast::<Direct3D12::ID3D12DebugDevice>() {
            debug_device.ReportLiveDeviceObjects(Direct3D12::D3D12_RLDO_FLAGS(
                Direct3D12::D3D12_RLDO_SUMMARY.0 | Direct3D12::D3D12_RLDO_IGNORE_INTERNAL.0,
            ));
            debug_device.destroy();
        }
    }

    #[allow(trivial_casts)]
    pub(super) fn expose(
        adapter: native::WeakPtr<Dxgi::IDXGIAdapter2>,
        library: &Arc<native::D3D12Lib>,
        instance_flags: crate::InstanceFlags,
    ) -> Option<crate::ExposedAdapter<super::Api>> {
        // Create the device so that we can get the capabilities.
        let device = {
            profiling::scope!("ID3D12Device::create_device");
            match library.create_device(adapter, native::FeatureLevel::L11_0) {
                Ok(pair) => match pair {
                    Ok(device) => device,
                    Err(err) => {
                        log::warn!("Device creation failed: {}", err);
                        return None;
                    }
                },
                Err(err) => {
                    log::warn!("Device creation function is not found: {:?}", err);
                    return None;
                }
            }
        };

        profiling::scope!("feature queries");

        // We have found a possible adapter.
        // Acquire the device information.
        let mut desc = unsafe { adapter.GetDesc2().unwrap() };

        let device_name = {
            use std::{ffi::OsString, os::windows::ffi::OsStringExt};
            let len = desc.Description.iter().take_while(|&&c| c != 0).count();
            let name = OsString::from_wide(&desc.Description[..len]);
            name.to_string_lossy().into_owned()
        };

        let mut features_architecture: Direct3D12::D3D12_FEATURE_DATA_ARCHITECTURE =
            unsafe { mem::zeroed() };
        unsafe {
            device
                .CheckFeatureSupport(
                    Direct3D12::D3D12_FEATURE_ARCHITECTURE,
                    &mut features_architecture as *mut _ as *mut _,
                    mem::size_of::<Direct3D12::D3D12_FEATURE_DATA_ARCHITECTURE>() as _,
                )
                .expect("Feature support check failed: D3D12_FEATURE_ARCHITECTURE")
        };

        let mut workarounds = super::Workarounds::default();

        let info = wgt::AdapterInfo {
            backend: wgt::Backend::Dx12,
            name: device_name,
            vendor: desc.VendorId as usize,
            device: desc.DeviceId as usize,
            device_type: if (desc.Flags & Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE.0) != 0 {
                workarounds.avoid_cpu_descriptor_overwrites = true;
                wgt::DeviceType::VirtualGpu
            } else if features_architecture.CacheCoherentUMA.0 != 0 {
                wgt::DeviceType::IntegratedGpu
            } else {
                wgt::DeviceType::DiscreteGpu
            },
        };

        let mut options: Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS = unsafe { mem::zeroed() };
        unsafe {
            device
                .CheckFeatureSupport(
                    Direct3D12::D3D12_FEATURE_D3D12_OPTIONS,
                    &mut options as *mut _ as *mut _,
                    mem::size_of::<Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS>() as _,
                )
                .expect("Feature support check failed: D3D12_FEATURE_D3D12_OPTIONS")
        }

        let _depth_bounds_test_supported = {
            let mut features2: Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS2 =
                unsafe { mem::zeroed() };
            let hr = unsafe {
                device.CheckFeatureSupport(
                    Direct3D12::D3D12_FEATURE_D3D12_OPTIONS2,
                    &mut features2 as *mut _ as *mut _,
                    mem::size_of::<Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS2>() as _,
                )
            };
            hr.is_ok() && features2.DepthBoundsTestSupported.0 != 0
        };

        //Note: `D3D12_FEATURE_D3D12_OPTIONS3::CastingFullyTypedFormatSupported` can be checked
        // to know if we can skip "typeless" formats entirely.

        let private_caps = super::PrivateCapabilities {
            instance_flags,
            heterogeneous_resource_heaps: options.ResourceHeapTier
                != Direct3D12::D3D12_RESOURCE_HEAP_TIER_1,
            memory_architecture: if features_architecture.UMA.0 != 0 {
                super::MemoryArchitecture::Unified {
                    cache_coherent: features_architecture.CacheCoherentUMA.0 != 0,
                }
            } else {
                super::MemoryArchitecture::NonUnified
            },
            heap_create_not_zeroed: false, //TODO: winapi support for Options7
        };

        // Theoretically vram limited, but in practice 2^20 is the limit
        let tier3_practical_descriptor_limit = 1 << 20;

        let (full_heap_count, _uav_count) = match options.ResourceBindingTier {
            Direct3D12::D3D12_RESOURCE_BINDING_TIER_1 => (
                Direct3D12::D3D12_MAX_SHADER_VISIBLE_DESCRIPTOR_HEAP_SIZE_TIER_1,
                8, // conservative, is 64 on feature level 11.1
            ),
            Direct3D12::D3D12_RESOURCE_BINDING_TIER_2 => (
                Direct3D12::D3D12_MAX_SHADER_VISIBLE_DESCRIPTOR_HEAP_SIZE_TIER_2,
                64,
            ),
            Direct3D12::D3D12_RESOURCE_BINDING_TIER_3 => (
                tier3_practical_descriptor_limit,
                tier3_practical_descriptor_limit,
            ),
            other => {
                log::warn!("Unknown resource binding tier {}", other.0);
                (
                    Direct3D12::D3D12_MAX_SHADER_VISIBLE_DESCRIPTOR_HEAP_SIZE_TIER_1,
                    8,
                )
            }
        };

        let mut features = wgt::Features::empty()
            | wgt::Features::DEPTH_CLAMPING
            | wgt::Features::MAPPABLE_PRIMARY_BUFFERS
            //TODO: Naga part
            //| wgt::Features::TEXTURE_BINDING_ARRAY
            //| wgt::Features::BUFFER_BINDING_ARRAY
            //| wgt::Features::STORAGE_RESOURCE_BINDING_ARRAY
            //| wgt::Features::UNSIZED_BINDING_ARRAY
            | wgt::Features::MULTI_DRAW_INDIRECT
            | wgt::Features::MULTI_DRAW_INDIRECT_COUNT
            | wgt::Features::ADDRESS_MODE_CLAMP_TO_BORDER
            | wgt::Features::POLYGON_MODE_LINE
            | wgt::Features::POLYGON_MODE_POINT
            | wgt::Features::VERTEX_WRITABLE_STORAGE
            | wgt::Features::TIMESTAMP_QUERY
            | wgt::Features::TEXTURE_COMPRESSION_BC
            | wgt::Features::CLEAR_COMMANDS;
        //TODO: in order to expose this, we need to run a compute shader
        // that extract the necessary statistics out of the D3D12 result.
        // Alternatively, we could allocate a buffer for the query set,
        // write the results there, and issue a bunch of copy commands.
        //| wgt::Features::PIPELINE_STATISTICS_QUERY

        features.set(
            wgt::Features::CONSERVATIVE_RASTERIZATION,
            options.ConservativeRasterizationTier
                != Direct3D12::D3D12_CONSERVATIVE_RASTERIZATION_TIER_NOT_SUPPORTED,
        );

        let base = wgt::Limits::default();

        Some(crate::ExposedAdapter {
            adapter: super::Adapter {
                raw: adapter,
                device,
                library: Arc::clone(library),
                private_caps,
                workarounds,
            },
            info,
            features,
            capabilities: crate::Capabilities {
                limits: wgt::Limits {
                    max_texture_dimension_1d: Direct3D12::D3D12_REQ_TEXTURE1D_U_DIMENSION,
                    max_texture_dimension_2d: Direct3D12::D3D12_REQ_TEXTURE2D_U_OR_V_DIMENSION
                        .min(Direct3D12::D3D12_REQ_TEXTURECUBE_DIMENSION),
                    max_texture_dimension_3d: Direct3D12::D3D12_REQ_TEXTURE3D_U_V_OR_W_DIMENSION,
                    max_texture_array_layers: Direct3D12::D3D12_REQ_TEXTURE2D_ARRAY_AXIS_DIMENSION,
                    max_bind_groups: crate::MAX_BIND_GROUPS as u32,
                    // dynamic offsets take a root constant, so we expose the minimum here
                    max_dynamic_uniform_buffers_per_pipeline_layout: base
                        .max_dynamic_uniform_buffers_per_pipeline_layout,
                    max_dynamic_storage_buffers_per_pipeline_layout: base
                        .max_dynamic_storage_buffers_per_pipeline_layout,
                    max_sampled_textures_per_shader_stage: match options.ResourceBindingTier {
                        Direct3D12::D3D12_RESOURCE_BINDING_TIER_1 => 128,
                        _ => full_heap_count,
                    },
                    max_samplers_per_shader_stage: match options.ResourceBindingTier {
                        Direct3D12::D3D12_RESOURCE_BINDING_TIER_1 => 16,
                        _ => Direct3D12::D3D12_MAX_SHADER_VISIBLE_SAMPLER_HEAP_SIZE,
                    },
                    // these both account towards `uav_count`, but we can't express the limit as as sum
                    max_storage_buffers_per_shader_stage: base.max_storage_buffers_per_shader_stage,
                    max_storage_textures_per_shader_stage: base
                        .max_storage_textures_per_shader_stage,
                    max_uniform_buffers_per_shader_stage: full_heap_count,
                    max_uniform_buffer_binding_size:
                        Direct3D12::D3D12_REQ_CONSTANT_BUFFER_ELEMENT_COUNT * 16,
                    max_storage_buffer_binding_size: !0,
                    max_vertex_buffers: Direct3D12::D3D12_VS_INPUT_REGISTER_COUNT
                        .min(crate::MAX_VERTEX_BUFFERS as u32),
                    max_vertex_attributes: Direct3D12::D3D12_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT,
                    max_vertex_buffer_array_stride: Direct3D12::D3D12_SO_BUFFER_MAX_STRIDE_IN_BYTES,
                    max_push_constant_size: 0,
                    min_uniform_buffer_offset_alignment:
                        Direct3D12::D3D12_CONSTANT_BUFFER_DATA_PLACEMENT_ALIGNMENT,
                    min_storage_buffer_offset_alignment: 4,
                    max_compute_workgroup_size_x: Direct3D12::D3D12_CS_THREAD_GROUP_MAX_X,
                    max_compute_workgroup_size_y: Direct3D12::D3D12_CS_THREAD_GROUP_MAX_Y,
                    max_compute_workgroup_size_z: Direct3D12::D3D12_CS_THREAD_GROUP_MAX_Z,
                    max_compute_workgroups_per_dimension:
                        Direct3D12::D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION,
                    // TODO?
                },
                alignments: crate::Alignments {
                    buffer_copy_offset: wgt::BufferSize::new(
                        Direct3D12::D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64,
                    )
                    .unwrap(),
                    buffer_copy_pitch: wgt::BufferSize::new(
                        Direct3D12::D3D12_TEXTURE_DATA_PITCH_ALIGNMENT as u64,
                    )
                    .unwrap(),
                },
                downlevel: wgt::DownlevelCapabilities::default(),
            },
        })
    }
}

impl crate::Adapter<super::Api> for super::Adapter {
    unsafe fn open(
        &self,
        features: wgt::Features,
        _limits: &wgt::Limits,
    ) -> Result<crate::OpenDevice<super::Api>, crate::DeviceError> {
        let queue = {
            profiling::scope!("ID3D12Device::CreateCommandQueue");
            self.device
                .create_command_queue(
                    native::CmdListType::Direct,
                    native::Priority::Normal,
                    native::CommandQueueFlags::empty(),
                    0,
                )
                .into_device_result("Queue creation")?
        };

        let device = super::Device::new(
            self.device,
            queue,
            features,
            self.private_caps,
            &self.library,
        )?;
        Ok(crate::OpenDevice {
            device,
            queue: super::Queue {
                raw: queue,
                temp_lists: Vec::new(),
            },
        })
    }

    #[allow(trivial_casts)]
    unsafe fn texture_format_capabilities(
        &self,
        format: wgt::TextureFormat,
    ) -> crate::TextureFormatCapabilities {
        use crate::TextureFormatCapabilities as Tfc;

        let raw_format = conv::map_texture_format(format);
        let mut data = Direct3D12::D3D12_FEATURE_DATA_FORMAT_SUPPORT {
            Format: raw_format,
            Support1: mem::zeroed(),
            Support2: mem::zeroed(),
        };
        self.device
            .CheckFeatureSupport(
                Direct3D12::D3D12_FEATURE_FORMAT_SUPPORT,
                &mut data as *mut _ as *mut _,
                mem::size_of::<Direct3D12::D3D12_FEATURE_DATA_FORMAT_SUPPORT>() as _,
            )
            .unwrap();

        let mut caps = Tfc::COPY_SRC | Tfc::COPY_DST;
        let can_image = 0
            != data.Support1.0
                & (Direct3D12::D3D12_FORMAT_SUPPORT1_TEXTURE1D.0
                    | Direct3D12::D3D12_FORMAT_SUPPORT1_TEXTURE2D.0
                    | Direct3D12::D3D12_FORMAT_SUPPORT1_TEXTURE3D.0
                    | Direct3D12::D3D12_FORMAT_SUPPORT1_TEXTURECUBE.0);
        caps.set(Tfc::SAMPLED, can_image);
        caps.set(
            Tfc::SAMPLED_LINEAR,
            data.Support1.0 & Direct3D12::D3D12_FORMAT_SUPPORT1_SHADER_SAMPLE.0 != 0,
        );
        caps.set(
            Tfc::COLOR_ATTACHMENT,
            data.Support1.0 & Direct3D12::D3D12_FORMAT_SUPPORT1_RENDER_TARGET.0 != 0,
        );
        caps.set(
            Tfc::COLOR_ATTACHMENT_BLEND,
            data.Support1.0 & Direct3D12::D3D12_FORMAT_SUPPORT1_BLENDABLE.0 != 0,
        );
        caps.set(
            Tfc::DEPTH_STENCIL_ATTACHMENT,
            data.Support1.0 & Direct3D12::D3D12_FORMAT_SUPPORT1_DEPTH_STENCIL.0 != 0,
        );
        caps.set(
            Tfc::STORAGE,
            data.Support1.0 & Direct3D12::D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0 != 0,
        );
        caps.set(
            Tfc::STORAGE_READ_WRITE,
            data.Support2.0 & Direct3D12::D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD.0 != 0,
        );

        caps
    }

    unsafe fn surface_capabilities(
        &self,
        surface: &super::Surface,
    ) -> Option<crate::SurfaceCapabilities> {
        let current_extent = {
            let mut rect: Foundation::RECT = mem::zeroed();
            if GetClientRect(Foundation::HWND(surface.wnd_handle as isize), &mut rect)
                != Foundation::BOOL(0)
            {
                Some(wgt::Extent3d {
                    width: (rect.right - rect.left) as u32,
                    height: (rect.bottom - rect.top) as u32,
                    depth_or_array_layers: 1,
                })
            } else {
                log::warn!("Unable to get the window client rect");
                None
            }
        };

        let mut present_modes = vec![wgt::PresentMode::Fifo];
        #[allow(trivial_casts)]
        if let Ok(factory5) = surface.factory.cast::<Dxgi::IDXGIFactory5>().into_result() {
            let mut allow_tearing: Foundation::BOOL = Foundation::BOOL(0);
            let hr = factory5.CheckFeatureSupport(
                Dxgi::DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                &mut allow_tearing as *mut _ as *mut _,
                mem::size_of::<Foundation::BOOL>() as _,
            );

            factory5.destroy();
            match hr.into_result() {
                Err(err) => log::warn!("Unable to check for tearing support: {}", err),
                Ok(()) => present_modes.push(wgt::PresentMode::Immediate),
            }
        }

        Some(crate::SurfaceCapabilities {
            formats: vec![
                wgt::TextureFormat::Bgra8UnormSrgb,
                wgt::TextureFormat::Bgra8Unorm,
                wgt::TextureFormat::Rgba8UnormSrgb,
                wgt::TextureFormat::Rgba8Unorm,
                wgt::TextureFormat::Rgb10a2Unorm,
                wgt::TextureFormat::Rgba16Float,
            ],
            // we currently use a flip effect which supports 2..=16 buffers
            swap_chain_sizes: 2..=16,
            current_extent,
            // TODO: figure out the exact bounds
            extents: wgt::Extent3d {
                width: 16,
                height: 16,
                depth_or_array_layers: 1,
            }..=wgt::Extent3d {
                width: 4096,
                height: 4096,
                depth_or_array_layers: 1,
            },
            usage: crate::TextureUses::COLOR_TARGET
                | crate::TextureUses::COPY_SRC
                | crate::TextureUses::COPY_DST,
            present_modes,
            composite_alpha_modes: vec![
                crate::CompositeAlphaMode::Opaque,
                crate::CompositeAlphaMode::PreMultiplied,
                crate::CompositeAlphaMode::PostMultiplied,
            ],
        })
    }
}
