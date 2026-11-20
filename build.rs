fn main() -> std::io::Result<()> {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        return Err(std::io::Error::other(
            "WinSizeP supports x64 only; build with --target x86_64-pc-windows-msvc",
        ));
    }
    let icon = "assets/winsizep.ico";
    println!("cargo:rerun-if-changed={icon}");
    println!("cargo:rerun-if-changed=assets/winsizep.manifest");
    // Cargo.toml [package].version is the sole product-version source.
    let version = std::env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let numbers = version.split(['-', '+']).next().unwrap_or(&version);
    let manifest =
        include_str!("assets/winsizep.manifest").replace("@VERSION@", &format!("{numbers}.0"));
    winresource::WindowsResource::new()
        .set_icon(icon)
        .set_manifest(&manifest)
        .set("CompanyName", "None")
        .set("Author", "Mikachu2333")
        .set("License", "MIT")
        .set(
            "LegalCopyright",
            "Copyright (c) 2026 Mikachu2333. MIT License",
        )
        .set("FileDescription", "Fluent Windows Adjust Modulator")
        .set("ProductName", "WinSizeP")
        .add_toolkit_include(true)
        .compile_for("winsizep")
}
