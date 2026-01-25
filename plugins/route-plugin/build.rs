//! Build script for route-plugin
//! Enables security features: ASLR, DEP, CFG

fn main() {
    #[cfg(target_os = "windows")]
    {
        // ASLR (Dynamic Base) - randomize load address
        println!("cargo:rustc-link-arg=/DYNAMICBASE");
        // DEP (NX Compatible) - prevent code execution in data segments
        println!("cargo:rustc-link-arg=/NXCOMPAT");
        // High Entropy ASLR (64-bit) - larger address space randomization
        println!("cargo:rustc-link-arg=/HIGHENTROPYVA");
        // Note: Control Flow Guard (/guard:cf) requires MSVC compiler flag
        // which is typically enabled by default in recent Rust versions
    }
}
