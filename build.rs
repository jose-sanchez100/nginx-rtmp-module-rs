//! Windows resources: 1 = executable / idle tray icon (drawn by
//! assets/make_icons.py), 2 = tray icon while a stream is live, version
//! information and an application manifest. Complete metadata also keeps
//! antivirus heuristics from treating the unsigned binary as anonymous.

fn main() {
    println!("cargo:rerun-if-changed=assets/nginx.ico");
    println!("cargo:rerun-if-changed=assets/nginx-live.ico");
    println!("cargo:rerun-if-changed=assets/nginx.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/nginx.ico");
        res.set_icon_with_id("assets/nginx-live.ico", "2");
        res.set("FileDescription", "RTMP server (Rust port of nginx-rtmp-module)");
        res.set("ProductName", "nginx-rtmp-module-rs");
        res.set("CompanyName", "nginx-rtmp-module-rs");
        res.set("InternalName", "nginx");
        res.set("OriginalFilename", "nginx.exe");
        res.set("LegalCopyright", "BSD 2-Clause License, see LICENSE");
        res.set("Comments", "https://github.com/jose-sanchez100/nginx-rtmp-module-rs");
        res.set_manifest_file("assets/nginx.manifest");
        res.compile().expect("failed to embed the Windows resources");
    }
}
