//! Headless audit of delivered PC definitions and their Lua-to-render ordering.
use anyhow::{ensure, Result};
fn main() -> Result<()> {
    let mut rows = Vec::new();
    for path in std::env::args().skip(1) {
        let pc = ferrite_portrayal_catalog::PortrayalCatalogue::load(&path)?;
        let mut refs: Vec<_> = pc.display_planes.planes.keys().collect();
        refs.sort();
        let mut planes = Vec::new();
        for name in refs {
            let encoded = name
                .replace('&', "&a")
                .replace(';', "&s")
                .replace(':', "&c")
                .replace(',', "&m");
            let parsed = ferrite_lua::parse_instruction_string(
                "audit",
                &format!("DisplayPlane:{encoded};PointInstruction:A"),
            )?;
            let plane = &parsed.commands[0].visibility().unwrap().display_plane;
            ensure!(
                plane.reference() == Some(name.as_str()),
                "identifier was changed"
            );
            let order = pc.display_planes.resolve(plane.reference().unwrap())?;
            let render = ferrite_render::DisplayPlane::from_catalogue_order(order);
            ensure!(render.order() == order, "order was changed");
            planes.push(serde_json::json!({"reference":name,"order":order.get(),"lua_reference":plane.reference(),"render_plane":render}));
        }
        rows.push(serde_json::json!({"path":path,"product":pc.product_id,"version":pc.version,"planes":planes}));
    }
    ensure!(!rows.is_empty(), "Pass catalogue directories");
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}
