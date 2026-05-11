use std::path::Path;

use anyhow::{Context, Result};
use image::GenericImageView;

/// Decoded image kept in RGBA8 form, ready for upload as a GL texture.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    pub fn load(path: &Path) -> Result<Self> {
        let img = image::open(path)
            .with_context(|| format!("decode image at {}", path.display()))?;
        let (width, height) = img.dimensions();
        let rgba = img.to_rgba8().into_raw();
        Ok(Self { width, height, rgba })
    }
}
