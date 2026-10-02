fn main() {
    slint_build::compile("ui/player.slint").expect("compile player UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("assets/maki.ico")
            .set("ProductName", "Just a music player")
            .set("FileDescription", "Just a music player")
            .set("LegalCopyright", "Copyright 2026 Maki contributors")
            .compile()
            .expect("embed Windows resources");
    }
    println!("cargo:rerun-if-changed=assets/maki.ico");
}
