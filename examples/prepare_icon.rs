use anyhow::Result;
use image::{
    codecs::ico::{IcoEncoder, IcoFrame},
    imageops::FilterType,
};
use std::{fs::OpenOptions, path::Path};
fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let image = image::open(root.join("assets/logo-v2.png"))?.into_rgba8();
    const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
    let frames = ICON_SIZES
        .into_iter()
        .map(|size| {
            let scaled = image::imageops::resize(&image, size, size, FilterType::Lanczos3);
            IcoFrame::as_png(&scaled, size, size, image::ExtendedColorType::Rgba8)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("assets/app.ico"))?;
    IcoEncoder::new(file).encode_images(&frames)?;
    println!(
        "Logo: {}x{}, corner alpha={}, ICO sizes={ICON_SIZES:?}",
        image.width(),
        image.height(),
        image.get_pixel(0, 0)[3]
    );
    Ok(())
}
