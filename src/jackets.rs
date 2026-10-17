use crate::files::{link_or_copy, write_if_changed};
use crate::layout::list_pngs;
use crate::par;
use crate::texture::encode_rgb_png;
use image::imageops::{blur, crop_imm, resize, FilterType};
use image::RgbImage;
use std::path::{Path, PathBuf};

const LOW_EDGE: u32 = 640;
const BLUR_EDGE: u32 = 1800;
const BLUR_SIGMA: f32 = 10.0;
/// The blur runs at this size and is scaled back up. A sigma-10 blur at 1800 px has no detail a
/// quarter-size pass loses: mean abs diff 0.33/255 over 25 jackets, 11x faster
const BLUR_WORK_EDGE: u32 = 450;
const BLUR_DIM: f32 = 0.62;

/// Derive low-res and blur images for every `illustration/**/*.png` (used when only the full images exist;
/// extract derives them straight from the decoded texture)
pub fn make_jackets(output: &Path) -> Result<usize, String> {
    let root = output.join("illustration");
    let pngs = list_pngs(&root);
    par::for_each(
        &pngs,
        par::threads(),
        || Ok(()),
        |_, _, src| {
            let name = src
                .strip_prefix(&root)
                .ok()
                .and_then(|n| n.to_str())
                .ok_or("jacket without a name")?;
            let img = image::open(src)
                .map_err(|e| format!("{}: {e}", src.display()))?
                .to_rgb8();
            derive(output, name, &img).map(|_| ())
        },
    )?;
    Ok(pngs.len())
}

/// Write `illustrationLowRes/<name>` (at most 640 px) and `illustrationBlur/<name>` (dimmed 1800 px
/// cover) from the full image already at `illustration/<name>`, and link all three under `original_ill/`
/// `name` is relative to those directories (`IN/<id>.png` for a per-difficulty jacket), so the levels
/// of one song never share an output. Returns every path it wrote or linked
pub fn derive(output: &Path, name: &str, img: &RgbImage) -> Result<Vec<PathBuf>, String> {
    let full = output.join("illustration").join(name);
    let low = output.join("illustrationLowRes").join(name);
    let blurred = output.join("illustrationBlur").join(name);
    write_if_changed(&low, &encode_rgb_png(&fit_inside(img, LOW_EDGE))?)?;
    let mut cover = blurred_cover(img, BLUR_EDGE);
    dim(&mut cover, BLUR_DIM);
    write_if_changed(&blurred, &encode_rgb_png(&cover)?)?;
    let mut written = Vec::with_capacity(5);
    for (src, web) in [(full, "ill"), (low, "illLow"), (blurred, "illBlur")] {
        let dest = output.join("original_ill").join(web).join(name);
        link_or_copy(&src, &dest)?;
        written.push(dest);
        if web != "ill" {
            written.push(src);
        }
    }
    Ok(written)
}

fn fit_inside(img: &RgbImage, edge: u32) -> RgbImage {
    let (w, h) = (img.width().max(1), img.height().max(1));
    if w <= edge && h <= edge {
        return img.clone();
    }
    let scale = (edge as f32 / w as f32).min(edge as f32 / h as f32);
    let nw = ((w as f32) * scale).round().max(1.0) as u32;
    let nh = ((h as f32) * scale).round().max(1.0) as u32;
    // Lanczos3, as before: CatmullRom is ~30% faster but changes every illLow and every pack's
    // thumbnail (mean abs diff 0.6/255, worst 2.2/255 over 314 jackets), and each changed object is
    // uploaded again, for about 0.2 s saved per run
    resize(img, nw, nh, FilterType::Lanczos3)
}

/// Centre square cropped from the source, blurred at `BLUR_WORK_EDGE` and scaled up to `edge`
fn blurred_cover(img: &RgbImage, edge: u32) -> RgbImage {
    let (w, h) = (img.width().max(1), img.height().max(1));
    let side = w.min(h);
    let square = crop_imm(img, (w - side) / 2, (h - side) / 2, side, side);
    let work = BLUR_WORK_EDGE.min(edge);
    let small = resize(&*square, work, work, FilterType::Triangle);
    let small = blur(&small, BLUR_SIGMA * work as f32 / edge as f32);
    resize(&small, edge, edge, FilterType::Triangle)
}

fn dim(img: &mut RgbImage, factor: f32) {
    let mut table = [0u8; 256];
    for (v, slot) in table.iter_mut().enumerate() {
        *slot = (v as f32 * factor).round().clamp(0.0, 255.0) as u8;
    }
    for c in img.iter_mut() {
        *c = table[*c as usize];
    }
}
