use anyhow::{ensure, Context, Result};
use ferrite_wgpu::coverage_clip::{create_clip_layout, fragment_clipped_shader, FragmentEntry};

async fn run() -> Result<()> {
    let source = include_str!("../crates/ferrite-wgpu/src/pipeline.rs");
    let instance = wgpu::Instance::new(&Default::default());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .context("No GPU adapter")?;
    let info = adapter.get_info();
    ensure!(
        matches!(
            info.device_type,
            wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::DiscreteGpu
        ),
        "Hardware GPU required: {:?}",
        info.device_type
    );
    let (device, _) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            },
            None,
        )
        .await?;
    let view = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(112),
            },
            count: None,
        }],
    });
    let asset = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    // Exercise the production factory with both target encodings and sample counts.
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    for format in [
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Rgba8UnormSrgb,
    ] {
        for samples in [1, 4] {
            let _legacy = ferrite_wgpu::coverage_pipeline::CoveragePipelines::new(
                &device, format, samples, &view, &asset, &asset,
            )?;
            let _instanced = ferrite_wgpu::coverage_pipeline::CoveragePipelines::new_with_symbol_instancing(
                &device, format, samples, &view, &asset, &asset, true,
            )?;
        }
    }
    device.poll(wgpu::Maintain::Wait);
    if let Some(error) = device.pop_error_scope().await {
        anyhow::bail!("Production coverage pipeline factory: {error}");
    }
    let clip = create_clip_layout(&device);
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[&view, &asset, &clip],
        push_constant_ranges: &[],
    });
    let mut checks = Vec::new();
    for (name, input, position, stride, attributes, entries) in [
        (
            "BASIC_SHADER",
            "VertexOutput",
            "clip_position",
            24,
            vec![
                (0, 0, wgpu::VertexFormat::Float32x2),
                (1, 8, wgpu::VertexFormat::Float32x4),
            ],
            vec!["fs_main"],
        ),
        (
            "LINE_SHADER",
            "VertexOutput",
            "clip_position",
            32,
            vec![
                (0, 0, wgpu::VertexFormat::Float32x2),
                (1, 8, wgpu::VertexFormat::Float32x2),
                (2, 16, wgpu::VertexFormat::Float32x4),
            ],
            vec!["fs_main"],
        ),
        (
            "TEXTURE_SHADER",
            "TextureVertexOutput",
            "clip_position",
            24,
            vec![
                (0, 0, wgpu::VertexFormat::Float32x2),
                (1, 8, wgpu::VertexFormat::Float32x2),
                (2, 16, wgpu::VertexFormat::Float32x2),
            ],
            vec!["fs_main"],
        ),
        (
            "PATTERN_FILL_SHADER",
            "PatternVertexOutput",
            "clip_position",
            24,
            vec![
                (0, 0, wgpu::VertexFormat::Float32x2),
                (1, 8, wgpu::VertexFormat::Float32x4),
            ],
            vec!["fs_main"],
        ),
        (
            "CHART_TEXT_SHADER",
            "Out",
            "position",
            20,
            vec![
                (0, 0, wgpu::VertexFormat::Float32x2),
                (1, 8, wgpu::VertexFormat::Float32x2),
                (2, 16, wgpu::VertexFormat::Unorm8x4),
            ],
            vec!["fs_gamma", "fs_linear"],
        ),
    ] {
        let marker = format!("const {name}: &str = r#\"");
        let shader = source
            .split_once(&marker)
            .with_context(|| format!("Missing {name}"))?
            .1
            .split_once("\"#;")
            .context("Missing shader end")?
            .0;
        let wrappers: Vec<_> = entries
            .iter()
            .map(|entry| FragmentEntry {
                name: entry,
                input_type: input,
                position_field: position,
            })
            .collect();
        let shader = fragment_clipped_shader(shader, 2, &wrappers)?;
        device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(name),
            source: wgpu::ShaderSource::Wgsl(shader.into()),
        });
        let attrs: Vec<_> = attributes
            .into_iter()
            .map(|(shader_location, offset, format)| wgpu::VertexAttribute {
                shader_location,
                offset,
                format,
            })
            .collect();
        for entry in entries {
            for samples in [1, 4] {
                let _pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(name),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[wgpu::VertexBufferLayout {
                            array_stride: stride,
                            step_mode: wgpu::VertexStepMode::Vertex,
                            attributes: &attrs,
                        }],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: wgpu::TextureFormat::Rgba8Unorm,
                            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState {
                        count: samples,
                        ..Default::default()
                    },
                    multiview: None,
                    cache: None,
                });
                checks.push(serde_json::json!({"shader":name,"entry":entry,"samples":samples}));
            }
        }
        device.poll(wgpu::Maintain::Wait);
        if let Some(error) = device.pop_error_scope().await {
            anyhow::bail!("{name}: {error}");
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"adapter":info.name,"backend":format!("{:?}",info.backend),"device_type":format!("{:?}",info.device_type),"pipeline_checks":checks,"production_factory_pipeline_checks":20,"application_rendering_verified":false})
        )?
    );
    Ok(())
}
fn main() -> Result<()> {
    pollster::block_on(run())
}
