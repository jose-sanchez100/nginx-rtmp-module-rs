//! Embed the executable icon (the nginx icon) on Windows builds.

fn main() {
    println!("cargo:rerun-if-changed=assets/rtmp-server.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/rtmp-server.ico");
        res.compile().expect("failed to embed the Windows icon resource");
    }
}
