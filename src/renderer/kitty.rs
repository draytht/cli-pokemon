use std::collections::HashSet;
use std::io::Write;

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use image::{imageops, RgbaImage};

use crate::sprite::gif::DecodedFrame;

const CHUNK_SIZE: usize = 4096;
/// Upload sprites at native 48×48; the terminal cell mapping handles scaling.
const TARGET_PX: u32 = 48;

pub struct KittyRenderer {
    uploaded: HashSet<u32>,
}

impl KittyRenderer {
    pub fn new() -> Self {
        Self {
            uploaded: HashSet::new(),
        }
    }

    /// Upload GIF frames as a Kitty multi-frame looping animation.
    /// `flip = true` mirrors frames horizontally (for left-facing walk).
    pub fn upload(
        &mut self,
        image_id: u32,
        frames: &[DecodedFrame],
        flip: bool,
        out: &mut impl Write,
    ) -> Result<()> {
        if self.uploaded.contains(&image_id) || frames.is_empty() {
            return Ok(());
        }

        for (i, frame) in frames.iter().enumerate() {
            let img = prepare_frame(&frame.pixels, flip);
            let rgba = img.as_raw();
            let encoded = B64.encode(rgba);
            let bytes = encoded.as_bytes();
            let chunks: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE).collect();
            let total = chunks.len();
            let w = img.width();
            let h = img.height();
            let gap = frame.delay_ms;

            for (ci, chunk) in chunks.iter().enumerate() {
                let more = if ci < total - 1 { 1u8 } else { 0u8 };
                let chunk_str = std::str::from_utf8(chunk).unwrap_or("");

                if i == 0 && ci == 0 {
                    // First chunk of first frame: transmit with full metadata
                    write!(
                        out,
                        "\x1b_Ga=T,f=32,i={},s={},v={},q=2,m={};{}\x1b\\",
                        image_id, w, h, more, chunk_str
                    )?;
                } else if i == 0 {
                    // Continuation of first frame
                    write!(out, "\x1b_Gm={};{}\x1b\\", more, chunk_str)?;
                } else if ci == 0 {
                    // First chunk of subsequent frame (with inter-frame gap)
                    write!(
                        out,
                        "\x1b_Ga=f,f=32,i={},z={},q=2,m={};{}\x1b\\",
                        image_id, gap, more, chunk_str
                    )?;
                } else {
                    write!(out, "\x1b_Gm={};{}\x1b\\", more, chunk_str)?;
                }
            }
        }

        // Start infinite loop playback
        write!(out, "\x1b_Ga=a,i={},s=3,v=1,q=2;\x1b\\", image_id)?;

        self.uploaded.insert(image_id);
        Ok(())
    }

    /// Place the animation at (col, row).
    /// Caller must handle erasing (delete-all once per frame for multi-pet).
    pub fn place(
        &mut self,
        image_id: u32,
        col: u16,
        row: u16,
        out: &mut impl Write,
    ) -> Result<()> {
        // Position cursor (1-indexed)
        write!(out, "\x1b[{};{}H", row + 1, col + 1)?;
        // Place the animation
        write!(out, "\x1b_Ga=p,i={},q=2;\x1b\\", image_id)?;
        Ok(())
    }

    pub fn delete_all(&self, out: &mut impl Write) -> Result<()> {
        write!(out, "\x1b_Ga=d,d=a,q=2;\x1b\\")?;
        Ok(())
    }

    pub fn on_resize(&mut self) {
        // Terminal cleared its image store on resize; re-upload everything
        self.uploaded.clear();
    }
}

fn prepare_frame(img: &RgbaImage, flip: bool) -> RgbaImage {
    let scaled = if img.width() == TARGET_PX && img.height() == TARGET_PX {
        img.clone()
    } else {
        imageops::resize(img, TARGET_PX, TARGET_PX, imageops::FilterType::Nearest)
    };
    if flip {
        imageops::flip_horizontal(&scaled)
    } else {
        scaled
    }
}

/// Stable image ID encoding (species, shiny, walk, flip) — always >= 1.
/// Reserves 4 bits per bool field, 12 bits for species.
pub fn image_id(species_id: u16, shiny: bool, is_walk: bool, flip: bool) -> u32 {
    let id = (species_id as u32) << 3
        | (shiny as u32) << 2
        | (is_walk as u32) << 1
        | (flip as u32);
    id.max(1)
}
