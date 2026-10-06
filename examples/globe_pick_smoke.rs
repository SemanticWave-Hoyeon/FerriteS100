//! Synthetic native ID picking: actual prepared CPU/GPU clips, alpha and depth.
use ferrite_kernel::{geodesy::GeographicPosition,globe_camera::GlobeCamera};
use ferrite_wgpu::{GpuState,globe_scene::{GlobeSceneRenderer,GlobeMesh,GlobeVertex,GlobeDraw,GlobeLayer,GlobeDepthMode}};
use std::{path::PathBuf,sync::Arc};
use winit::{application::ApplicationHandler,dpi::PhysicalSize,event::WindowEvent,event_loop::{ActiveEventLoop,EventLoop},window::{Window,WindowId}};
fn quad(c:&GlobeCamera,rect:[f64;4],texture:bool,color:[f32;4])->GlobeMesh {
    let [x,y,w,h]=rect;let positions=[[x,y],[x+w,y],[x+w,y+h],[x,y+h]];let uv=[[0.,0.,0.,1.],[1.,0.,0.,1.],[1.,1.,0.,1.],[0.,1.,0.,1.]];
    GlobeMesh {vertices:positions.into_iter().enumerate().map(|(i,p)|GlobeVertex {ecef_m:c.device_plane_point(p).unwrap(),color:if texture {uv[i]}else{color}}).collect(),indices:vec![0,1,2,0,2,3]}
}
struct App{out:PathBuf}
impl ApplicationHandler for App {
    fn resumed(&mut self,e:&ActiveEventLoop){
        std::fs::create_dir_all(&self.out).unwrap();let w=Arc::new(e.create_window(Window::default_attributes().with_title("Globe ID picking").with_inner_size(PhysicalSize::new(640,480))).unwrap());let gpu=pollster::block_on(GpuState::new(w)).unwrap();let size=[gpu.config.width,gpu.config.height];
        let layout=gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {label:None,entries:&[wgpu::BindGroupLayoutEntry{binding:0,visibility:wgpu::ShaderStages::FRAGMENT,ty:wgpu::BindingType::Texture {sample_type:wgpu::TextureSampleType::Float{filterable:true},view_dimension:wgpu::TextureViewDimension::D2,multisampled:false},count:None},wgpu::BindGroupLayoutEntry{binding:1,visibility:wgpu::ShaderStages::FRAGMENT,ty:wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),count:None}]});
        let texture=gpu.device.create_texture(&wgpu::TextureDescriptor {label:None,size:wgpu::Extent3d {width:2,height:2,depth_or_array_layers:1},mip_level_count:1,sample_count:1,dimension:wgpu::TextureDimension::D2,format:wgpu::TextureFormat::Rgba8Unorm,usage:wgpu::TextureUsages::TEXTURE_BINDING|wgpu::TextureUsages::COPY_DST,view_formats:&[]});
        gpu.queue.write_texture(wgpu::TexelCopyTextureInfo {texture:&texture,mip_level:0,origin:wgpu::Origin3d::ZERO,aspect:wgpu::TextureAspect::All},&[255,0,0,0,255,0,0,255,255,0,0,255,255,0,0,255],wgpu::TexelCopyBufferLayout {offset:0,bytes_per_row:Some(8),rows_per_image:Some(2)},wgpu::Extent3d {width:2,height:2,depth_or_array_layers:1});
        let view=texture.create_view(&Default::default());let sampler=gpu.device.create_sampler(&wgpu::SamplerDescriptor {mag_filter:wgpu::FilterMode::Nearest,min_filter:wgpu::FilterMode::Nearest,..Default::default()});let binding=gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {label:None,layout:&layout,entries:&[wgpu::BindGroupEntry{binding:0,resource:wgpu::BindingResource::TextureView(&view)},wgpu::BindGroupEntry{binding:1,resource:wgpu::BindingResource::Sampler(&sampler)}]});
        let mut checks=Vec::new();
        for gpu_projection in [false,true] { for (heading,tilt) in [(0.,0.),(90.,70.)] {
            let camera=GlobeCamera::orbit(GeographicPosition::new(48.,179.9).unwrap(),30000.,heading,tilt,size.map(|x|x as f64),45.,3.,1e9).unwrap();let mut scene=GlobeSceneRenderer::new_with_textures(&gpu.device,gpu.config.format,Some(&layout));scene.set_gpu_projection_enabled(gpu_projection);
            let base=quad(&camera,[80.,80.,240.,240.],false,[0.,1.,0.,1.]);let opaque=quad(&camera,[100.,100.,160.,160.],false,[1.,0.,0.,1.]);let transparent=quad(&camera,[100.,100.,160.,160.],false,[1.,0.,0.,0.]);let glyph=quad(&camera,[100.,100.,160.,160.],true,[1.;4]);
            let draws=[GlobeDraw{layer:GlobeLayer{mesh:&base,depth_mode:GlobeDepthMode::Occluder},texture:None,font_color:None},GlobeDraw{layer:GlobeLayer{mesh:&opaque,depth_mode:GlobeDepthMode::SurfaceOverlay},texture:None,font_color:None},GlobeDraw{layer:GlobeLayer{mesh:&transparent,depth_mode:GlobeDepthMode::SurfaceOverlay},texture:None,font_color:None},GlobeDraw{layer:GlobeLayer{mesh:&glyph,depth_mode:GlobeDepthMode::SurfaceOverlay},texture:Some(&binding),font_color:None}];
            scene.prepare_draws(&gpu.device,&gpu.queue,&camera,&draws).unwrap();
            for (point,expected) in [([120.5,120.5],1),([230.5,230.5],3),([90.5,90.5],0)] {
                let hits=scene.pick_draws(&gpu.device,&gpu.queue,point,0.).unwrap();assert_eq!(hits.len(),1);assert_eq!(hits[0].draw_index,expected);assert_eq!(hits[0].pixel,point);checks.push(serde_json::json!({"gpu_projection_requested":gpu_projection,"heading":heading,"tilt":tilt,"pixel":point,"draw_index":expected,"alpha_and_draw_order":true}));
            }
            assert!(scene.pick_draws(&gpu.device,&gpu.queue,[20.5,20.5],0.).unwrap().is_empty());
            let hits=scene.pick_draws(&gpu.device,&gpu.queue,[180.,180.],40.).unwrap();assert!(hits.iter().any(|h|h.draw_index==1));assert!(hits.iter().any(|h|h.draw_index==3));assert!(!hits.iter().any(|h|h.draw_index==2));
            // A mesh missing its central triangles is not selectable through its hole.
            let mut ring=quad(&camera,[100.,100.,160.,40.],false,[1.;4]);for rect in [[100.,220.,160.,40.],[100.,140.,40.,80.],[220.,140.,40.,80.]] {ring.append(&quad(&camera,rect,false,[1.;4])).unwrap();}
            scene.prepare_draws(&gpu.device,&gpu.queue,&camera,&[GlobeDraw{layer:GlobeLayer{mesh:&ring,depth_mode:GlobeDepthMode::SurfaceOverlay},texture:None,font_color:None}]).unwrap();assert!(scene.pick_draws(&gpu.device,&gpu.queue,[180.5,180.5],0.).unwrap().is_empty());assert_eq!(scene.pick_draws(&gpu.device,&gpu.queue,[120.5,180.5],0.).unwrap()[0].draw_index,0);
            // An occluder at the camera-facing plane hides real surface geometry.
            let center=GeographicPosition::new(48.,179.9).unwrap();let far=GlobeMesh {vertices:[center,GeographicPosition::new(48.001,179.9).unwrap(),GeographicPosition::new(48.,179.901).unwrap()].into_iter().map(|p|GlobeVertex{ecef_m:p.to_ecef(0.).unwrap(),color:[1.;4]}).collect(),indices:vec![0,1,2]};
            let full=quad(&camera,[0.,0.,size[0] as f64,size[1] as f64],false,[1.;4]);scene.prepare_draws(&gpu.device,&gpu.queue,&camera,&[GlobeDraw{layer:GlobeLayer{mesh:&full,depth_mode:GlobeDepthMode::Occluder},texture:None,font_color:None},GlobeDraw{layer:GlobeLayer{mesh:&far,depth_mode:GlobeDepthMode::SurfaceOverlay},texture:None,font_color:None}]).unwrap();let hit=scene.pick_draws(&gpu.device,&gpu.queue,[size[0] as f64/2.+0.5,size[1] as f64/2.+0.5],0.).unwrap();assert_eq!(hit[0].draw_index,0);
        }}
        std::fs::write(self.out.join("result.json"),serde_json::to_string_pretty(&serde_json::json!({"checks":checks,"hole_cases":4,"occlusion_cases":4,"empty_cases":4,"radius_cases":4,"single_sample_pixel_centers":true,"product_adapter_verified":false,"ui_connected":false})).unwrap()).unwrap();e.exit();
    }
    fn window_event(&mut self,_:&ActiveEventLoop,_:WindowId,_:WindowEvent){}
}
fn main(){let mut app=App{out:std::env::args().nth(1).map(PathBuf::from).expect("output")};EventLoop::new().unwrap().run_app(&mut app).unwrap();}
