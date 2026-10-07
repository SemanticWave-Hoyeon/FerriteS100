//! Lazily constructed masked variants of the existing chart shaders.
//! The caller binds group 2 per dataset. Solid draws additionally bind
//! `empty_asset` at group 1; textured draws retain their original asset group.
use crate::coverage_clip::{create_clip_layout, fragment_clipped_shader, FragmentEntry};
use crate::pipeline::{
    BASIC_SHADER, CHART_TEXT_SHADER, LINE_SHADER, PATTERN_FILL_SHADER, TEXTURE_SHADER,
};
use crate::{ChartTextVertex, LineVertex, PatternVertex, Result, TextureVertex, Vertex2D};

pub struct CoveragePipelines {
    pub area: wgpu::RenderPipeline,
    pub line: wgpu::RenderPipeline,
    pub symbol: wgpu::RenderPipeline,
    pub(crate) symbol_instance: Option<wgpu::RenderPipeline>,
    pub pattern: wgpu::RenderPipeline,
    pub text: wgpu::RenderPipeline,
    pub clip_layout: wgpu::BindGroupLayout,
    pub empty_asset: wgpu::BindGroup,
}
impl CoveragePipelines {
    /// Preserve the original six-argument API with instancing disabled.
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        samples: u32,
        view_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        pattern_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self> {
        Self::new_with_symbol_instancing(
            device,
            format,
            samples,
            view_layout,
            texture_layout,
            pattern_layout,
            false,
        )
    }

    /// Keep the ordinary pipelines unchanged. Construct these variants only
    /// when coverage selection has prepared dataset masks for a chart frame.
    pub fn new_with_symbol_instancing(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        samples: u32,
        view_layout: &wgpu::BindGroupLayout,
        texture_layout: &wgpu::BindGroupLayout,
        pattern_layout: &wgpu::BindGroupLayout,
        symbol_instancing: bool,
    ) -> Result<Self> {
        let clip_layout = create_clip_layout(device);
        let empty_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("coverage-empty-asset"),
            entries: &[],
        });
        let empty_asset = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("coverage-empty-asset"),
            layout: &empty_layout,
            entries: &[],
        });
        let basic_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("coverage-solid-layout"),
            bind_group_layouts: &[view_layout, &empty_layout, &clip_layout],
            push_constant_ranges: &[],
        });
        let symbol_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("coverage-texture-layout"),
            bind_group_layouts: &[view_layout, texture_layout, &clip_layout],
            push_constant_ranges: &[],
        });
        let pattern_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("coverage-pattern-layout"),
                bind_group_layouts: &[view_layout, pattern_layout, &clip_layout],
                push_constant_ranges: &[],
            });
        let solid = |name| {
            [FragmentEntry {
                name,
                input_type: "VertexOutput",
                position_field: "clip_position",
            }]
        };
        let make = |label,
                    source: &str,
                    layout: &wgpu::PipelineLayout,
                    vertex: wgpu::VertexBufferLayout<'_>,
                    entry: &str,
                    wrappers: &[FragmentEntry],
                    blend|
         -> Result<wgpu::RenderPipeline> {
            let source = fragment_clipped_shader(source, 2, wrappers)?;
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            Ok(
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[vertex],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(blend),
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
                }),
            )
        };
        let area = make(
            "coverage-area",
            BASIC_SHADER,
            &basic_layout,
            Vertex2D::desc(),
            "fs_main",
            &solid("fs_main"),
            wgpu::BlendState::ALPHA_BLENDING,
        )?;
        let line = make(
            "coverage-line",
            LINE_SHADER,
            &basic_layout,
            LineVertex::desc(),
            "fs_main",
            &solid("fs_main"),
            wgpu::BlendState::ALPHA_BLENDING,
        )?;
        let symbol = make(
            "coverage-symbol",
            TEXTURE_SHADER,
            &symbol_layout,
            TextureVertex::desc(),
            "fs_main",
            &[FragmentEntry {
                name: "fs_main",
                input_type: "TextureVertexOutput",
                position_field: "clip_position",
            }],
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        )?;
        let symbol_instance = if symbol_instancing {
            let symbol_instance = make(
                "coverage-symbol-instance",
                &crate::symbol_instance::shader(),
                &symbol_layout,
                crate::symbol_instance::SymbolQuadInstance::desc(),
                "fs_main",
                &[FragmentEntry {
                    name: "fs_main",
                    input_type: "TextureVertexOutput",
                    position_field: "clip_position",
                }],
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            )?;
            Some(symbol_instance)
        } else {
            None
        };
        let pattern = make(
            "coverage-pattern",
            PATTERN_FILL_SHADER,
            &pattern_pipeline_layout,
            PatternVertex::desc(),
            "fs_main",
            &[FragmentEntry {
                name: "fs_main",
                input_type: "PatternVertexOutput",
                position_field: "clip_position",
            }],
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        )?;
        let text = make(
            "coverage-text",
            CHART_TEXT_SHADER,
            &symbol_layout,
            ChartTextVertex::desc(),
            if format.is_srgb() {
                "fs_linear"
            } else {
                "fs_gamma"
            },
            &[
                FragmentEntry {
                    name: "fs_gamma",
                    input_type: "Out",
                    position_field: "position",
                },
                FragmentEntry {
                    name: "fs_linear",
                    input_type: "Out",
                    position_field: "position",
                },
            ],
            wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        )?;
        Ok(Self {
            area,
            line,
            symbol,
            symbol_instance,
            pattern,
            text,
            clip_layout,
            empty_asset,
        })
    }
}

#[cfg(test)]
mod api_compatibility_tests {
    use super::*;
    #[test]
    fn legacy_six_argument_and_opt_in_seven_argument_factory_signatures_compile() {
        type Legacy = fn(
            &wgpu::Device,
            wgpu::TextureFormat,
            u32,
            &wgpu::BindGroupLayout,
            &wgpu::BindGroupLayout,
            &wgpu::BindGroupLayout,
        ) -> Result<CoveragePipelines>;
        type OptIn = fn(
            &wgpu::Device,
            wgpu::TextureFormat,
            u32,
            &wgpu::BindGroupLayout,
            &wgpu::BindGroupLayout,
            &wgpu::BindGroupLayout,
            bool,
        ) -> Result<CoveragePipelines>;
        let _: Legacy = CoveragePipelines::new;
        let _: OptIn = CoveragePipelines::new_with_symbol_instancing;
    }
}
