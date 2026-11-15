//! Embed the icons on Windows builds: 1 = executable / idle tray icon (the
//! nginx icon), 2 = tray icon while a stream is live.

fn main() {
    println!("cargo:rerun-if-changed=assets/rtmp-server.ico");
    println!("cargo:rerun-if-changed=assets/rtmp-server-live.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rtmp-server.ico");
        res.set_icon_with_id("assets/rtmp-server-live.ico", "2");
        res.compile().expect("failed to embed the Windows icon resource");
    }
}
