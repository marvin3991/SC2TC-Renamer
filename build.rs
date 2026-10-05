use std::{env, path::Path};
fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let root = env::var("CARGO_MANIFEST_DIR").unwrap();
        if Path::new(&root).join("assets/app.ico").exists() {
            winresource::WindowsResource::new()
                .set_icon("assets/app.ico")
                .set("ProductName", "SC2TC-Renamer")
                .set("FileDescription", "Offline Chinese filename conversion")
                .compile()
                .expect("Windows icon resource compilation failed");
        }
        println!("cargo:rerun-if-changed=assets/app.ico");
    }
}
