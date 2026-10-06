//! Identity only: no constructor for a numerical/hardware qualification is added.
//! Private renderer-owned records cannot be supplied by application callers.
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedContinuousIdentity {
    pub(crate) camera: [u64;16],
    pub(crate) vertex_bytes: Vec<u8>,
    pub(crate) index_bytes: Vec<u8>,
    pub(crate) material: [u8;32],
}
#[derive(Clone)]
pub(crate) struct ContinuousFrameBinding {
    owner: Arc<()>, // Unique renderer/device lifetime. Never an address-only integer.
    uniforms: Vec<Vec<u8>>, // Actual central/left/right upload bytes, ordered.
    viewport: [u32;2],
    sample_count: u32,
    target_format: wgpu::TextureFormat,
    layers: Vec<PreparedContinuousIdentity>, // Ordered payloads: no hash-only equality.
}
impl ContinuousFrameBinding {
    pub(crate) fn capture(owner: &Arc<()>, uniforms: &[Vec<u8>], viewport:[u32;2],
        sample_count:u32, target_format:wgpu::TextureFormat, layers:Vec<PreparedContinuousIdentity>) -> Self {
        Self {owner:Arc::clone(owner), uniforms:uniforms.to_vec(), viewport,sample_count,target_format,layers}
    }
    pub(crate) fn matches_current<'a>(&self,owner:&Arc<()>,uniforms:impl Iterator<Item=&'a [u8]>,
        viewport:[u32;2],sample_count:u32,target_format:wgpu::TextureFormat,
        layers:impl Iterator<Item=&'a PreparedContinuousIdentity>)->bool {
        Arc::ptr_eq(&self.owner,owner) && self.viewport==viewport && self.sample_count==sample_count && self.target_format==target_format
            && self.uniforms.iter().map(Vec::as_slice).eq(uniforms) && self.layers.iter().eq(layers)
    }
    pub(crate) fn matches(&self, current:&Self)->bool {
        Arc::ptr_eq(&self.owner,&current.owner) && self.uniforms==current.uniforms && self.viewport==current.viewport
            && self.sample_count==current.sample_count && self.target_format==current.target_format && self.layers==current.layers
    }
}
#[cfg(test)] mod tests {
 use super::*;
 fn state(owner:&Arc<()>)->ContinuousFrameBinding {
  ContinuousFrameBinding::capture(owner,&[vec![0;16]],[800,600],4,wgpu::TextureFormat::Bgra8Unorm,
   vec![PreparedContinuousIdentity{camera:[0;16],vertex_bytes:vec![1,2,3],index_bytes:vec![0,1,2],material:[4;32]}])
 }
 #[test] fn actual_payload_and_camera_mutations_invalidate_equal_viewport_material() {
  let owner=Arc::new(());let baseline=state(&owner);assert!(baseline.matches(&state(&owner)));
  let mut changed=state(&owner);changed.layers[0].camera[3]=1;assert!(!baseline.matches(&changed));
  let mut changed=state(&owner);changed.layers[0].vertex_bytes[0]^=1;assert!(!baseline.matches(&changed));
  let mut changed=state(&owner);changed.layers[0].index_bytes.swap(0,1);assert!(!baseline.matches(&changed));
 }
 #[test] fn different_actual_scalers_cannot_reuse_identical_material_frame_binding() {
  use ferrite_render::{Scaler,GeoBounds,Viewport};
  let first=Scaler::new(GeoBounds::new(0.,0.,2.,2.),Viewport::new(800.,600.));
  let second=Scaler::new(GeoBounds::new(0.,0.,4.,2.),Viewport::new(800.,600.));
  let owner=Arc::new(()); let mut a=state(&owner);let mut b=state(&owner);
  a.layers[0].camera=first.flat_encoded_identity().unwrap();
  b.layers[0].camera=second.flat_encoded_identity().unwrap();
  assert_eq!(a.layers[0].material,b.layers[0].material);
  assert_eq!(a.viewport,b.viewport);assert!(!a.matches(&b));
 }
 #[test] fn backend_lifetime_uniform_sample_and_target_state_are_bound() {
  let owner=Arc::new(());let baseline=state(&owner);assert!(!baseline.matches(&state(&Arc::new(()))));
  let mut changed=state(&owner);changed.uniforms[0][0]=1;assert!(!baseline.matches(&changed));
  let mut changed=state(&owner);changed.sample_count=1;assert!(!baseline.matches(&changed));
  let mut changed=state(&owner);changed.target_format=wgpu::TextureFormat::Bgra8UnormSrgb;assert!(!baseline.matches(&changed));
  let mut changed=state(&owner);changed.layers.push(changed.layers[0].clone());assert!(!baseline.matches(&changed));
  let mut ordered=state(&owner);let mut second=ordered.layers[0].clone();second.material[0]^=1;second.vertex_bytes[0]^=1;ordered.layers.push(second);
  let mut swapped=ordered.clone();swapped.layers.swap(0,1);assert!(!ordered.matches(&swapped));
  assert!(ordered.matches_current(&owner,ordered.uniforms.iter().map(Vec::as_slice),ordered.viewport,ordered.sample_count,ordered.target_format,ordered.layers.iter()));
  assert!(!ordered.matches_current(&owner,swapped.uniforms.iter().map(Vec::as_slice),swapped.viewport,swapped.sample_count,swapped.target_format,swapped.layers.iter()));
 }
}
