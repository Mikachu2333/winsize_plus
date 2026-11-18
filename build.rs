fn main() -> std::io::Result<()> {
    let icon = "assets/winsizep.ico";
    println!("cargo:rerun-if-changed={icon}");
    winresource::WindowsResource::new()
        .set_icon(icon)
        .compile_for("winsizep")
}
