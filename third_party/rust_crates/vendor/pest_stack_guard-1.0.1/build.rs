fn main() {
    println!("cargo:rustc-check-cfg=cfg(native_stack_guard)");
    let target = std::env::var("TARGET").expect("Cargo must provide TARGET");
    // Match the exact dependency targets in Cargo.toml, including vendor and ABI.
    if matches!(
        target.as_str(),
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-apple-darwin"
            | "aarch64-apple-darwin"
            | "x86_64-pc-windows-msvc"
            | "aarch64-pc-windows-msvc"
    ) {
        println!("cargo:rustc-cfg=native_stack_guard");
    }
}
