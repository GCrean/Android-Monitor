fn main() {
    // Embed the app icon (resource id 1, which GPUI uses for the window/taskbar icon) on Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        #[cfg(windows)]
        {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "Android Monitor");
            res.set("FileDescription", "Android Monitor");
            if let Err(e) = res.compile() {
                println!("cargo:warning=could not embed icon: {e}");
            }
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
