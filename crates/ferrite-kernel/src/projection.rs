//! Checked WGS84 UTM/UPS point operations, independent of product encodings.
//! Native values are named easting/northing in metres; geographic angles use
//! named latitude/longitude in degrees. Explicit EPSG parameters never choose
//! an automatic UTM zone. Point conversion is not a whole-footprint enclosure
//! certificate, a vertical-datum transform, or a rendering precision bound.
use anyhow::{ensure, Result};
use crate::geodesy::GeographicPosition;
use std::sync::OnceLock;
mod vendor;
use vendor::{transverse_mercator::TransverseMercator,polar_stereographic::PolarStereographic};
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
enum Kind {Utm{zone:u8,north:bool},Ups{north:bool}}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct Wgs84ProjectedCrs {epsg:u32,kind:Kind}
#[derive(Debug,Clone,Copy,PartialEq)]
pub struct NativeProjectedPosition {crs:Wgs84ProjectedCrs,easting:f64,northing:f64}
#[derive(Debug,Clone,Copy)]
pub struct GeographicProjectionResult {
    pub source:NativeProjectedPosition,
    pub position:GeographicPosition,
    /// False at a UPS pole; the returned longitude is the convention zero.
    pub longitude_defined:bool,
    /// A point-level rectangular area check, not dataset/product certification.
    pub within_crs_area:bool,
}
#[derive(Debug,Clone,Copy)]
pub struct NativeProjectionResult {
    pub source:GeographicPosition,
    pub position:NativeProjectedPosition,
    pub within_crs_area:bool,
}
fn tm()-> &'static TransverseMercator {static T:OnceLock<TransverseMercator>=OnceLock::new();T.get_or_init(TransverseMercator::utm)}
fn ps()-> &'static PolarStereographic {static P:OnceLock<PolarStereographic>=OnceLock::new();P.get_or_init(PolarStereographic::ups)}
impl Wgs84ProjectedCrs {
    /// WGS84 UTM 1..60 N/S and UPS (E,N). Axis-reversed UPS aliases and other
    /// datums are deliberately not silently treated as these coordinate pairs.
    pub fn from_epsg(epsg:u32)->Result<Self> {
        let kind=match epsg {
            32601..=32660=>Kind::Utm{zone:(epsg-32600) as u8,north:true},
            32701..=32760=>Kind::Utm{zone:(epsg-32700) as u8,north:false},
            5041=>Kind::Ups{north:true},5042=>Kind::Ups{north:false},
            _=>anyhow::bail!("Unsupported WGS84 projected EPSG:{epsg}"),
        };Ok(Self{epsg,kind})
    }
    pub fn epsg(self)->u32 {self.epsg}
    fn false_origin(self)->(f64,f64) {match self.kind {Kind::Utm{north,..}=>(500_000.,if north {0.}else{10_000_000.}),Kind::Ups{..}=>(2_000_000.,2_000_000.)}}
    fn delta(self,p:GeographicPosition)->Result<f64> {let Kind::Utm{zone,..}=self.kind else{return Ok(0.);};let meridian=6.*f64::from(zone)-183.;Ok(p.longitude_near(meridian)?-meridian)}
    fn within_area(self,p:GeographicPosition)->Result<bool> {Ok(match self.kind {
        Kind::Utm{north,..}=>self.delta(p)?.abs()<=3. && if north {(0. ..=84.).contains(&p.latitude())}else{(-80. ..=0.).contains(&p.latitude())},
        Kind::Ups{north}=>if north {p.latitude()>=60.}else{p.latitude()<=-60.},
    })}
    /// Bounded numerical profile: UTM |latitude|<=84 and <=10 degrees from
    /// the explicit meridian; UPS >=60 degrees in the selected hemisphere.
    /// The computational profile includes points outside the CRS area; those
    /// are labelled, not normalized, assigned another zone, or certified.
    pub fn project(self,source:GeographicPosition)->Result<NativeProjectionResult> {
        let (x,y)=match self.kind {
            Kind::Utm{zone,..}=>{
                ensure!(source.latitude().abs()<=84. && self.delta(source)?.abs()<=10.,"Outside supported UTM numerical profile");
                tm().from_latlon(6.*f64::from(zone)-183.,source.latitude(),source.longitude())
            },
            Kind::Ups{north}=>{
                ensure!(if north {source.latitude()>=60.}else{source.latitude()<=-60.},"Outside supported UPS numerical profile");
                ps().from_latlon(north,source.latitude(),source.longitude())
            },
        };
        let (fe,fnorth)=self.false_origin();let position=NativeProjectedPosition::new(self,x+fe,y+fnorth)?;
        Ok(NativeProjectionResult{source,position,within_crs_area:self.within_area(source)?})
    }
}
impl NativeProjectedPosition {
    /// Finite bounded computational coordinates; stored values are unchanged.
    pub fn new(crs:Wgs84ProjectedCrs,easting:f64,northing:f64)->Result<Self> {
        ensure!(easting.is_finite()&&northing.is_finite(),"Nonfinite projected coordinate");
        let (fe,fnorth)=crs.false_origin();let x=easting-fe;let y=northing-fnorth;
        ensure!(match crs.kind {Kind::Utm{..}=>x.abs()<=1_500_000.&&y.abs()<=9_500_000.,Kind::Ups{..}=>x.hypot(y)<=4_000_000.},"Outside supported inverse projection numerical profile");
        Ok(Self{crs,easting,northing})
    }
    pub fn crs(self)->Wgs84ProjectedCrs {self.crs}
    pub fn easting(self)->f64 {self.easting}
    pub fn northing(self)->f64 {self.northing}
    /// Inverse admission has a margin around the forward domain: UTM
    /// |latitude|<=85/meridian delta<=12; UPS >=59 in the selected hemisphere.
    /// This avoids making a rounded result at a forward boundary invalid.
    pub fn to_geographic(self)->Result<GeographicProjectionResult> {
        let (fe,fnorth)=self.crs.false_origin();let x=self.easting-fe;let y=self.northing-fnorth;
        let (raw,longitude_defined)=match self.crs.kind {
            Kind::Utm{zone,..}=>(tm().to_latlon(6.*f64::from(zone)-183.,x,y),true),
            Kind::Ups{north}=>(ps().to_latlon(north,x,y),x!=0.||y!=0.),
        };
        let position=GeographicPosition::new(raw.latitude,if longitude_defined {raw.longitude}else{0.})?;
        match self.crs.kind {
            Kind::Utm{..}=>ensure!(position.latitude().abs()<=85.&&self.crs.delta(position)?.abs()<=12.,"Outside supported inverse UTM numerical profile"),
            Kind::Ups{north}=>ensure!(if north {position.latitude()>=59.}else{position.latitude()<=-59.},"Outside supported inverse UPS numerical profile"),
        };
        Ok(GeographicProjectionResult{source:self,position,longitude_defined,within_crs_area:self.crs.within_area(position)?})
    }
}
#[cfg(test)] mod tests {
 use super::*;
 #[test] fn explicit_zone_and_false_origins_cover_all_utm_codes() {
  for zone in 1..=60 {
   let lon=6.*f64::from(zone)-183.;
   for (base,northing) in [(32600,0.),(32700,10_000_000.)] {
    let c=Wgs84ProjectedCrs::from_epsg(base+zone).unwrap();let p=GeographicPosition::new(0.,lon).unwrap();let n=c.project(p).unwrap();
    assert_eq!(n.position.crs().epsg(),base+zone);assert_eq!(n.position.easting(),500_000.);assert_eq!(n.position.northing(),northing);
    let g=n.position.to_geographic().unwrap();assert_eq!(g.position,p);assert_eq!(g.source,n.position);assert!(g.longitude_defined);
   }
  }
 }
 #[test] fn ups_poles_preserve_native_source_and_mark_indeterminate_longitude() {
  for (epsg,lat) in [(5041,90.),(5042,-90.)] {
   let c=Wgs84ProjectedCrs::from_epsg(epsg).unwrap();
   for lon in [-180.,-90.,0.,90.,180.] {
    let n=c.project(GeographicPosition::new(lat,lon).unwrap()).unwrap();assert_eq!(n.position.easting(),2_000_000.);assert_eq!(n.position.northing(),2_000_000.);
    let g=n.position.to_geographic().unwrap();assert_eq!(g.position.latitude(),lat);assert_eq!(g.position.longitude(),0.);assert!(!g.longitude_defined);assert_eq!(g.source,n.position);
   }
  }
 }
 #[test] fn explicit_zone_does_not_reassign_nor_conflate_numeric_and_area_domains() {
  let p=GeographicPosition::new(60.,5.).unwrap();let c=Wgs84ProjectedCrs::from_epsg(32631).unwrap();let n=c.project(p).unwrap();assert_eq!(n.position.crs().epsg(),32631);assert!(n.within_crs_area);
  let n=c.project(GeographicPosition::new(60.,8.).unwrap()).unwrap();assert!(!n.within_crs_area);assert_eq!(n.position.crs().epsg(),32631);
  let n=Wgs84ProjectedCrs::from_epsg(32631).unwrap().project(GeographicPosition::new(-45.,3.).unwrap()).unwrap();assert!(n.position.northing()<0.);assert!(!n.within_crs_area);
 }
 #[test] fn invalid_codes_nonfinite_antipodal_and_overflow_inputs_are_rejected() {
  for e in [0,4326,32600,32661,32761,3857,9999] {assert!(Wgs84ProjectedCrs::from_epsg(e).is_err());}
  let c=Wgs84ProjectedCrs::from_epsg(32631).unwrap();
  for (e,n) in [(f64::NAN,0.),(0.,f64::INFINITY),(1e300,0.),(0.,1e300)] {assert!(NativeProjectedPosition::new(c,e,n).is_err());}
  for (lat,lon) in [(0.,-177.),(90.,3.),(-90.,3.)] {assert!(c.project(GeographicPosition::new(lat,lon).unwrap()).is_err());}
  assert!(Wgs84ProjectedCrs::from_epsg(5041).unwrap().project(GeographicPosition::new(-85.,0.).unwrap()).is_err());
 }
 #[test] fn independent_proj_reference_matches_forward_and_inverse_in_all_supported_codes() {
  #[derive(serde::Deserialize)] struct Reference {pyproj:String,proj:String,cases:Vec<Row>}
  #[derive(serde::Deserialize)] struct Row {epsg:u32,latitude:f64,longitude:f64,easting:f64,northing:f64,inverse_latitude:f64,inverse_longitude:f64,longitude_defined:bool,forward_supported:bool}
  let data:Reference=serde_json::from_str(include_str!("projection/proj_reference.json")).unwrap();assert_eq!(data.cases.len(),3162);
  let mut max_metres=0f64;let mut max_degrees=0f64;
  for r in &data.cases {
   let crs=Wgs84ProjectedCrs::from_epsg(r.epsg).unwrap();let p=GeographicPosition::new(r.latitude,r.longitude).unwrap();if r.forward_supported {let actual=crs.project(p).unwrap();
   let error=(actual.position.easting()-r.easting).abs().max((actual.position.northing()-r.northing).abs());max_metres=max_metres.max(error);
   assert!(error<=1e-5,"EPSG:{} lat/lon {},{} forward error {}m",r.epsg,r.latitude,r.longitude,error);}
   let native=NativeProjectedPosition::new(crs,r.easting,r.northing).unwrap();let inverse=native.to_geographic().unwrap();assert_eq!(inverse.source,native);assert_eq!(inverse.longitude_defined,r.longitude_defined);
   let error_lat=(inverse.position.latitude()-r.inverse_latitude).abs();
   let error_lon=if r.longitude_defined {((inverse.position.longitude()-r.inverse_longitude+180.).rem_euclid(360.)-180.).abs()}else{0.};
   let error=error_lat.max(error_lon);max_degrees=max_degrees.max(error);
   assert!(error<=1e-9,"EPSG:{} E/N {},{} inverse error {}deg",r.epsg,r.easting,r.northing,error);
  }
  println!("Independent PROJ {} / pyproj {}: {} cases; maximum forward error {}m; maximum inverse angular error {}deg",data.proj,data.pyproj,data.cases.len(),max_metres,max_degrees);
 }

 #[test] fn ups_epsg_area_is_distinct_from_utm_ups_operational_switch() {
  for (epsg,sign) in [(5041,1.),(5042,-1.)] {
   let c=Wgs84ProjectedCrs::from_epsg(epsg).unwrap();
   for lat in [60.,73.,80.,84.,90.] {assert!(c.project(GeographicPosition::new(sign*lat,0.).unwrap()).unwrap().within_crs_area);}
   assert!(c.project(GeographicPosition::new(sign*59.5,0.).unwrap()).is_err());
  }
 }

}
