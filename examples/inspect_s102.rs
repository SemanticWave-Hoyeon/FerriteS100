use anyhow::Result;
use ferrite_s102::hdf5::{
    self,
    types::{VarLenAscii, VarLenUnicode},
};
fn walk(g: &hdf5::Group) -> Result<()> {
    println!("GROUP {}", g.name());
    for n in g.attr_names()? {
        let a = g.attr(&n)?;
        let t = a.dtype()?;
        let desc = t.to_descriptor()?;
        let val = if t.is::<VarLenAscii>() {
            format!("{:?}", a.read_raw::<VarLenAscii>()?)
        } else if t.is::<VarLenUnicode>() {
            format!("{:?}", a.read_raw::<VarLenUnicode>()?)
        } else if matches!(
            desc,
            hdf5::types::TypeDescriptor::Integer(_)
                | hdf5::types::TypeDescriptor::Unsigned(_)
                | hdf5::types::TypeDescriptor::Float(_)
        ) {
            format!("{:?}", a.read_raw::<f64>()?)
        } else {
            format!("{:?}", desc)
        };
        println!(" ATTR {n}: {val}");
    }
    for n in g.member_names()? {
        if let Ok(child) = g.group(&n) {
            walk(&child)?;
        } else if let Ok(d) = g.dataset(&n) {
            println!(
                " DATA {} {:?} {:?}",
                d.name(),
                d.shape(),
                d.dtype()?.to_descriptor()?
            );
        }
    }
    Ok(())
}
fn main() -> Result<()> {
    let f = hdf5::File::open(std::env::args().nth(1).expect("H5 file"))?;
    walk(&f)?;
    Ok(())
}
