//! Shared selector material prototype for flat colour
//! must call this SAME function before emitting anything. This module is staged;
//! uploader and shared material entrypoints are wired, but frame promotion remains
//! disabled until projection/interpolation and hardware precision are qualified.
//! Inputs are tile-local common-cell coordinates, not preselected centroid pixels.
//! Geometry/interpolant qualification must establish <=1/64 physical-pixel error;
//! this material adds <=1/64 pixel uncertainty. Near a domain/cell edge BOTH
//! incident candidates are eligible and the valid minimum rank wins. This is a
//! conservative display band, NOT exact f64 robust-predicate parity.
pub(crate) const SELECTOR_WGSL: &str = r#"
struct ContinuousSelection { rgba:vec4<f32>, source:u32, column:u32, row:u32, rank:u32 };
fn selector_word(image:texture_2d<f32>, index:u32)->u32 {
 let size=textureDimensions(image);
 let p=vec2<i32>(i32(index%size.x),i32(index/size.x));
 let b=vec4<u32>(round(textureLoad(image,p,0)*255.));
 return b.x | (b.y<<8u) | (b.z<<16u) | (b.w<<24u);
}
fn selector_point(image:texture_2d<f32>, index:u32)->vec2<f32> {
 return vec2<f32>(bitcast<f32>(selector_word(image,index)),bitcast<f32>(selector_word(image,index+1u)));
}
fn selector_domain(image:texture_2d<f32>, origin:u32,count:u32,p:vec2<f32>,epsilon:f32)->bool {
 if count==0u {return true;}
 var winding=0i;
 for(var i=0u;i<count;i=i+1u) {
  let a=selector_point(image,origin+2u*i);
  let b=selector_point(image,origin+2u*((i+1u)%count));
  let d=b-a;let q=p-a;let side=d.x*q.y-d.y*q.x;
  let edge_length=max(length(d),1e-30);
  // Closed domains and a qualified conservative near-edge display band.
  if abs(side)<=epsilon*edge_length && all(p>=min(a,b)-vec2<f32>(epsilon)) && all(p<=max(a,b)+vec2<f32>(epsilon)) {return true;}
  if a.y<=p.y && b.y>p.y && side>0. {winding=winding+1i;}
  if a.y>p.y && b.y<=p.y && side<0. {winding=winding-1i;}
 }
 return winding!=0i;
}
// span is abs(dpdx(local_coords))+abs(dpdy(local_coords)), evaluated by the
// fragment entry point BEFORE dynamic material branches (uniformity contract).
// coordinate_error must be an independently qualified geometry/interpolant bound
// in source-cell units; it is not inferred from polygon-centre tests.
fn continuous_grid_select(image:texture_2d<f32>,p:vec2<f32>,span:vec2<f32>,coordinate_error:f32)->ContinuousSelection {
 var out:ContinuousSelection;out.rgba=vec4<f32>(0.);out.source=0xffffffffu;out.column=0xffffffffu;out.row=0xffffffffu;out.rank=0xffffffffu;
 if selector_word(image,0u)!=0x53444331u || selector_word(image,1u)!=1u {return out;}
 let count=selector_word(image,2u);let tile=vec2<u32>(selector_word(image,5u),selector_word(image,6u));
 let extent=vec2<f32>(f32(selector_word(image,7u)),f32(selector_word(image,8u)));
 if count>32u || any(p<vec2<f32>(0.)) || any(p>extent) {return out;}
 let cast_error=bitcast<f32>(selector_word(image,10u));
 let domain_magnitude=bitcast<f32>(selector_word(image,12u));
 let eps=2.*cast_error+coordinate_error+16.*1.1920929e-7*max(max(max(abs(p.x),abs(p.y)),domain_magnitude),1.);
 // Reject an unqualified fragment rather than selecting an arbitrary subset.
 // Host qualification must diagnose this BEFORE drawing; this guard is defence.
 if eps>=0.25 || any(span<=vec2<f32>(0.)) || eps/min(span.x,span.y)>1./64. {return out;}
 for(var source=0u;source<count;source=source+1u) {
  let h=16u+source*16u;
  let ratio=vec2<u32>(selector_word(image,h),selector_word(image,h+1u));
  let begin=vec2<u32>(selector_word(image,h+4u),selector_word(image,h+5u));
  let size=vec2<u32>(selector_word(image,h+6u),selector_word(image,h+7u));
  let data=selector_word(image,h+8u);let points=selector_word(image,h+9u);let vertices=selector_word(image,h+10u);
  if !selector_domain(image,points,vertices,p,eps) {continue;}
  // Integer quotient/remainder before conversion avoids large global-grid f32
  // coordinates. Original source cells, including boundary halos, are retained.
  let q=vec2<f32>(tile%ratio)/vec2<f32>(ratio)+p/vec2<f32>(ratio)+vec2<f32>(tile/ratio-begin);
  let base=vec2<i32>(floor(q));var other=base;
  let fraction=fract(q);let e=vec2<f32>(eps)/vec2<f32>(ratio);
  other=select(other,base-vec2<i32>(1),fraction<=e);
  other=select(other,base+vec2<i32>(1),vec2<f32>(1.)-fraction<=e);
  for(var ix=0u;ix<2u;ix=ix+1u) {for(var iy=0u;iy<2u;iy=iy+1u) {
   let cell=vec2<i32>(select(base.x,other.x,ix==1u),select(base.y,other.y,iy==1u));
   if any(cell<vec2<i32>(0)) || any(cell>=vec2<i32>(size)) {continue;}
   let local=u32(cell.y)*size.x+u32(cell.x);let rank=selector_word(image,data+local*2u);
   if rank>=out.rank {continue;}
   let colour=selector_word(image,data+local*2u+1u);
   out.rgba=vec4<f32>(f32(colour&255u),f32((colour>>8u)&255u),f32((colour>>16u)&255u),f32(colour>>24u))/255.;
   let original=begin+vec2<u32>(cell);let dims=vec2<u32>(selector_word(image,h+2u),selector_word(image,h+3u));
   let flags=selector_word(image,h+11u);
   let column=select(original.x,dims.x-1u-original.x,(flags&1u)!=0u);
   let row=select(original.y,dims.y-1u-original.y,(flags&2u)!=0u);
   out.source=source;out.column=column;out.row=row;out.rank=rank;
  }}
 }
 return out;
}
"#;

/// Opaque promotion token. There is intentionally NO constructor while whole-
/// triangle projection/interpolation and hardware precision remain unvalidated.
/// Staging/compiling a material must not silently turn it into a frame certificate.
#[derive(Clone)]
pub struct ValidatedContinuousFrame {
    pub(crate) transform_key:[u32;9],
    pub(crate) binding:crate::continuous_frame_binding::ContinuousFrameBinding,
    pub(crate) material_keys:Vec<[u8;32]>,
    pub(crate) coordinate_error_source:f32,
    _qualified_by_backend:(),
}
impl ValidatedContinuousFrame {
    pub(crate) fn error(&self)->f32 {self.coordinate_error_source}
}

/// A separate flat material leaves the existing raster shader/pipeline unchanged.
/// The selector function below is composed into flat colour.
pub(crate) fn flat_shader()->String {
    format!("{}\n{}",SELECTOR_WGSL,r#"
struct Input { @location(0) position:vec2<f32>, @location(1) anchor:vec2<f32>, @location(2) origin:vec2<f32>, @location(3) step:vec2<f32>, @location(4) offset:vec2<u32>, @location(5) size:vec2<u32>, @location(6) row_bounds:vec2<f32>, @location(7) row_index:u32 }
struct Output { @builtin(position) position:vec4<f32>, @location(0) @interpolate(flat) origin:vec2<f32>, @location(1) @interpolate(flat) step:vec2<f32> }
struct ViewUniforms {view_proj:mat4x4<f32>,viewport_size:vec2<f32>,scale:f32,_padding:f32,pan_offset:vec2<f32>,zoom_scale:f32,zoom_scale_y:f32,zoom_pivot:vec2<f32>,_padding3:vec2<f32>}
@group(0) @binding(0) var<uniform> view:ViewUniforms;
@group(1) @binding(0) var pixels:texture_2d<f32>;
@vertex fn vs_main(v:Input)->Output {
 var o:Output;
 let anchor=(v.anchor+view.pan_offset-view.zoom_pivot)*vec2<f32>(view.zoom_scale,view.zoom_scale_y)+view.zoom_pivot;
 o.origin=(v.origin+view.pan_offset-view.zoom_pivot)*vec2<f32>(view.zoom_scale,view.zoom_scale_y)+view.zoom_pivot;
 o.step=v.step*vec2<f32>(view.zoom_scale,view.zoom_scale_y);
 let far=o.origin+o.step*vec2<f32>(v.size);
 o.position=view.view_proj*vec4<f32>(clamp(anchor+(v.position-v.anchor),o.origin,far),0.,1.);return o;
}
@fragment fn fs_main(v:Output)->@location(0) vec4<f32> {
 let global=(v.position.xy-v.origin)/v.step;
 let span=abs(dpdx(global))+abs(dpdy(global));
 let tile=vec2<u32>(selector_word(pixels,5u),selector_word(pixels,6u));
 let size=vec2<u32>(selector_word(pixels,7u),selector_word(pixels,8u));
 // One shared global interpolant owns each output cell/fragment. Original source
 // closed boundaries are compared inside the selector, never by this floor rule.
 let owner=vec2<i32>(floor(global));
 if any(owner<vec2<i32>(tile)) || any(owner>=vec2<i32>(tile+size)) {discard;}
 if selector_word(pixels,14u)!=1u {discard;}
 let selected=continuous_grid_select(pixels,global-vec2<f32>(tile),span,bitcast<f32>(selector_word(pixels,13u)));
 if selected.rank==0xffffffffu || selected.rgba.a<=0. {discard;}
 return selected.rgba;
}
"#)
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn shared_flat_selector_parses_and_validates_without_gpu_or_window() {
        let source=flat_shader();let module=wgpu::naga::front::wgsl::parse_str(&source).unwrap();
        wgpu::naga::valid::Validator::new(wgpu::naga::valid::ValidationFlags::all(),wgpu::naga::valid::Capabilities::all()).validate(&module).unwrap();
        assert!(source.contains("selected.rgba.a<=0."));assert!(source.contains("selected.rank==0xffffffffu"));
    }
}
