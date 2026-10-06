//! Independent UI-panel geometry and pixel-density oracle on the native GPU.
use ferrite_wgpu::{AppUiState, EguiIntegration, GpuState, PluginButton};
use std::{path::PathBuf, sync::Arc};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowId},
};
struct Smoke {
    out: PathBuf,
}
fn save(
    state: &GpuState,
    egui: &mut EguiIntegration,
    output: egui::FullOutput,
    path: &std::path::Path,
) -> image::RgbImage {
    let width = 2400u32;
    let height = 1600u32;
    let texture = state.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("UI layout oracle"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: state.format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let mut encoder = state.device.create_command_encoder(&Default::default());
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.,
                        g: 0.,
                        b: 1.,
                        a: 1.,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
    }
    let bytes_per_row = (width * 4).div_ceil(256) * 256;
    let buffer = state.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: bytes_per_row as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    // Deliberately pass native density 1: render must use the output's real UI density.
    egui.render(
        &state.device,
        &state.queue,
        &mut encoder,
        &view,
        egui_wgpu::ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: 1.,
        },
        output,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    state.queue.submit(Some(encoder.finish()));
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        tx.send(r).unwrap();
    });
    state.device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    let data = buffer.slice(..).get_mapped_range();
    let mut image = image::RgbImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let offset = (y * bytes_per_row + x * 4) as usize;
            let p = &data[offset..offset + 4];
            let rgb = if matches!(
                state.format(),
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
            ) {
                [p[2], p[1], p[0]]
            } else {
                [p[0], p[1], p[2]]
            };
            image.put_pixel(x, y, image::Rgb(rgb));
        }
    }
    image.save(path).unwrap();
    image
}
impl ApplicationHandler for Smoke {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let window = Arc::new(
            el.create_window(
                Window::default_attributes()
                    .with_inner_size(PhysicalSize::new(2400, 1600))
                    .with_title("Measured chart layout oracle"),
            )
            .unwrap(),
        );
        let state = pollster::block_on(GpuState::new(window.clone())).unwrap();
        let mut integration = EguiIntegration::new(&state.device, state.format(), 1, window);
        std::fs::create_dir_all(&self.out).unwrap();
        let mut checks = Vec::new();
        for density in [1f32, 1.5, 2.] {
            for panel in [240f32, 480.] {
                for route in [false, true] {
                    let ctx = integration.ctx.clone();
                    ctx.set_pixels_per_point(density);
                    ctx.data_mut(|data| {
                        data.insert_persisted(
                            egui::Id::new("feature_panel"),
                            egui::containers::panel::PanelState {
                                rect: egui::Rect::from_min_size(
                                    egui::Pos2::ZERO,
                                    egui::vec2(panel, 500.),
                                ),
                            },
                        )
                    });
                    let mut ui = AppUiState::default();
                    if route {
                        ui.plugin_buttons.push(PluginButton {
                            plugin_id: "route-fixture".into(),
                            label: "Route".into(),
                            active: true,
                            ..Default::default()
                        });
                    }
                    let mut output: Option<egui::FullOutput> = None;
                    for _ in 0..2 {
                        ctx.begin_pass(egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(2400. / density, 1600. / density),
                            )),
                            ..Default::default()
                        });
                        integration.draw_ui(&mut ui);
                        let frame = ctx.end_pass();
                        if let Some(previous) = &mut output {
                            previous.append(frame);
                        } else {
                            output = Some(frame);
                        }
                    }
                    let output = output.unwrap();
                    let actual_density = output.pixels_per_point;
                    assert!((actual_density - density).abs() < 0.001);
                    let right = egui::containers::panel::PanelState::load(
                        &ctx,
                        egui::Id::new("feature_panel"),
                    )
                    .unwrap()
                    .rect;
                    let top =
                        egui::containers::panel::PanelState::load(&ctx, egui::Id::new("toolbar"))
                            .unwrap()
                            .rect;
                    let bottom = egui::containers::panel::PanelState::load(
                        &ctx,
                        egui::Id::new("status_bar"),
                    )
                    .unwrap()
                    .rect;
                    let (x, y, w, h) = ui.chart_area;
                    assert!((x + w - right.min.x).abs() < 0.01);
                    assert!((y - top.max.y).abs() < 0.01);
                    assert!((y + h - bottom.min.y).abs() < 0.01);
                    if route {
                        let left = egui::containers::panel::PanelState::load(
                            &ctx,
                            egui::Id::new("route_panel"),
                        )
                        .unwrap()
                        .rect;
                        assert!((x - left.max.x).abs() < 0.01);
                    } else {
                        assert_eq!(x, 0.);
                    }
                    let image = save(
                        &state,
                        &mut integration,
                        output,
                        &self.out.join(format!("{density}-{panel}-{route}.png")),
                    );
                    let center = [
                        ((x + w / 2.) * density) as u32,
                        ((y + h / 2.) * density) as u32,
                    ];
                    assert_eq!(
                        image.get_pixel(center[0], center[1]).0,
                        [0, 0, 255],
                        "Chart center covered by mis-scaled UI"
                    );
                    let side = [
                        ((right.min.x + right.width() / 2.) * density) as u32,
                        ((right.min.y + right.height() / 2.) * density) as u32,
                    ];
                    assert_ne!(
                        image.get_pixel(side[0], side[1]).0,
                        [0, 0, 255],
                        "Feature panel was drawn at wrong density"
                    );
                    checks.push(serde_json::json!({"ui_density":density,"requested_panel_width":panel,"measured_panel_width":right.width(),"route":route,"chart_rect_logical":[x,y,w,h],"chart_center_physical":center,"feature_panel_probe_physical":side}));
                }
            }
        }
        std::fs::write(
            self.out.join("result.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"cases":checks,"native_os_input_verified":false}),
            )
            .unwrap(),
        )
        .unwrap();
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}
fn main() {
    EventLoop::new()
        .unwrap()
        .run_app(&mut Smoke {
            out: std::env::args().nth(1).unwrap().into(),
        })
        .unwrap();
}
