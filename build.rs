fn main() -> std::io::Result<()> {
    let icon = "assets/winsizep.ico";
    println!("cargo:rerun-if-changed={icon}");
    println!("cargo:rerun-if-changed=assets/settings.rc");
    winresource::WindowsResource::new()
        .set_icon(icon)
        .add_toolkit_include(true)
        .append_rc_content(include_str!("assets/settings.rc"))
        .compile_for("winsizep")
}
