//! Gives the desktop app its icon: resource 1 is what the window, taskbar
//! and Explorer show on Windows.

fn main() {
    println!("cargo:rerun-if-changed=assets/helajica.rc");
    println!("cargo:rerun-if-changed=assets/helajica.ico");
    #[cfg(windows)]
    embed_resource::compile_for("assets/helajica.rc", ["helajica"], embed_resource::NONE)
        .manifest_optional()
        .expect("embed the app icon");
}
