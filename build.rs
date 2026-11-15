//! Embed the icons on Windows builds: 1 = executable / idle tray icon (drawn by
//! assets/make_icons.py), 2 = tray icon while a stream is live.

fn main() {
    println!("cargo:rerun-if-changed=assets/nginx.ico");
    println!("cargo:rerun-if-changed=assets/nginx-live.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/nginx.ico");
        res.set_icon_with_id("assets/nginx-live.ico", "2");
        res.compile().expect("failed to embed the Windows icon resource");
    }
}
