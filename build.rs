fn main() {
    // Only re-run when icon or this build script changes
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icon.ico");

    // Only run on Windows
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().expect("Failed to compile Windows resources");
    }
}
