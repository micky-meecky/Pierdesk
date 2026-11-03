//! Pixel-based candidates from Snow Shot's GPL visual detector. The input is
//! the frozen screenshot, never the live desktop underneath the editor.
use visual_region_detector::{BgrImage, detect_regions};
use windows::Win32::Foundation::RECT;

use super::candidates::Region;

#[allow(clippy::many_single_char_names)] // Pixel rectangle arithmetic uses x/y/w/h consistently.
pub(super) fn detect(
    pixels: &[u8],
    bounds: RECT,
    root: Region,
    mut progress: impl FnMut(&[Region]),
) -> Vec<Region> {
    let Ok(width) = usize::try_from(bounds.right - bounds.left) else {
        return Vec::new();
    };
    let Ok(height) = usize::try_from(bounds.bottom - bounds.top) else {
        return Vec::new();
    };
    let Ok(x) = usize::try_from(root.x) else {
        return Vec::new();
    };
    let Ok(y) = usize::try_from(root.y) else {
        return Vec::new();
    };
    let Ok(w) = usize::try_from(root.w) else {
        return Vec::new();
    };
    let Ok(h) = usize::try_from(root.h) else {
        return Vec::new();
    };
    if w == 0
        || h == 0
        || x + w > width
        || y + h > height
        || width
            .checked_mul(height)
            .and_then(|size| size.checked_mul(4))
            != Some(pixels.len())
    {
        return Vec::new();
    }
    // Bound CPU and scratch buffers independently of virtual-desktop size.
    let step = w.max(h).div_ceil(1600);
    let out_w = w.div_ceil(step);
    let out_h = h.div_ceil(step);
    let mut bgr = Vec::with_capacity(out_w * out_h * 3);
    for row in (y..y + h).step_by(step) {
        for col in (x..x + w).step_by(step) {
            let offset = (row * width + col) * 4;
            bgr.extend_from_slice(&pixels[offset..offset + 3]);
        }
    }
    let Ok(image) = BgrImage::from_bgr_bytes(
        u32::try_from(out_w).unwrap_or(0),
        u32::try_from(out_h).unwrap_or(0),
        bgr,
    ) else {
        return Vec::new();
    };
    let step = i32::try_from(step).unwrap_or(1);
    let map = |r: visual_region_detector::Rect| {
        let x = root.x + r.x * step;
        let y = root.y + r.y * step;
        let w = (r.w * step).min(root.x + root.w - x);
        let h = (r.h * step).min(root.y + root.h - y);
        (w >= 24 && h >= 24).then_some(Region { x, y, w, h })
    };
    let pictures: Vec<_> = surface_components(&image)
        .into_iter()
        .filter_map(map)
        .collect();
    progress(&pictures);
    let mut output = pictures.clone();
    if let Ok(regions) = detect_regions(&image) {
        output.extend(
            regions
                .into_iter()
                .filter_map(|region| map(region.rect))
                .filter(|region| {
                    // A picture's internal game text/widgets are not separate GUI
                    // controls. Prefer its outer boundary over contained fragments.
                    !pictures.iter().any(|picture| {
                        let overlap_w = (region.x + region.w).min(picture.x + picture.w)
                            - region.x.max(picture.x);
                        let overlap_h = (region.y + region.h).min(picture.y + picture.h)
                            - region.y.max(picture.y);
                        let overlap = i64::from(overlap_w.max(0)) * i64::from(overlap_h.max(0));
                        let area = i64::from(region.w) * i64::from(region.h);
                        let picture_area = i64::from(picture.w) * i64::from(picture.h);
                        overlap * 100 >= (area + picture_area - overlap) * 90
                            || overlap * 100 >= area * 85 && area * 100 < picture_area * 95
                    })
                })
                .take(2048),
        );
    }
    output
}

// Complement the upstream texture detector with large regions separated by
// flat application surfaces. No OCR or application-specific coordinates.
fn surface_components(image: &BgrImage) -> Vec<visual_region_detector::Rect> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let pixels = image.pixels();
    let mut histogram = vec![0usize; 32768];
    for pixel in pixels {
        histogram[usize::from(pixel[0] >> 3) * 1024
            + usize::from(pixel[1] >> 3) * 32
            + usize::from(pixel[2] >> 3)] += 1;
    }
    let backgrounds: Vec<_> = histogram
        .iter()
        .enumerate()
        .filter(|(_, count)| **count > pixels.len() / 40)
        .map(|(index, _)| {
            [
                i16::try_from((index / 1024) * 8 + 4).unwrap(),
                i16::try_from(((index / 32) % 32) * 8 + 4).unwrap(),
                i16::try_from((index % 32) * 8 + 4).unwrap(),
            ]
        })
        .collect();
    if backgrounds.is_empty() {
        return Vec::new();
    }
    let mut mask: Vec<_> = pixels
        .iter()
        .map(|pixel| {
            u8::from(backgrounds.iter().all(|color| {
                pixel
                    .iter()
                    .zip(color)
                    .any(|(a, b)| (i16::from(*a) - b).abs() > 4)
            }))
        })
        .collect();
    let mut stack = Vec::new();
    let mut regions = Vec::new();
    for start in 0..pixels.len() {
        if mask[start] == 0 {
            continue;
        }
        mask[start] = 0;
        stack.push(start);
        let (mut left, mut right, mut top, mut bottom) =
            (start % width, start % width, start / width, start / width);
        let mut area = 0usize;
        while let Some(index) = stack.pop() {
            let x = index % width;
            let y = index / width;
            area += 1;
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
            for next in [
                x.checked_sub(1).map(|_| index - 1),
                (x + 1 < width).then_some(index + 1),
                y.checked_sub(1).map(|_| index - width),
                (y + 1 < height).then_some(index + width),
            ]
            .into_iter()
            .flatten()
            {
                if mask[next] != 0 {
                    mask[next] = 0;
                    stack.push(next);
                }
            }
        }
        let w = right - left + 1;
        let h = bottom - top + 1;
        if w >= 64 && h >= 64 && area * 100 >= w * h * 35 && w * h * 100 < pixels.len() * 85 {
            regions.push(visual_region_detector::Rect::new(
                i32::try_from(left).unwrap(),
                i32::try_from(top).unwrap(),
                i32::try_from(w).unwrap(),
                i32::try_from(h).unwrap(),
            ));
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_frame_is_rejected_without_detecting_live_content() {
        assert!(
            detect(
                &[],
                RECT {
                    right: 100,
                    bottom: 100,
                    ..RECT::default()
                },
                Region {
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 100
                },
                |_| {}
            )
            .is_empty()
        );
    }

    #[test]
    #[ignore = "Reads only explicitly supplied private screenshot fixtures; never stores them in source control"]
    fn supplied_screenshot_images_are_individual_candidates() {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
        };
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .unwrap();
        for path in std::env::split_paths(
            &std::env::var_os("PIERDESK_REGION_FIXTURES").expect("Set PIERDESK_REGION_FIXTURES"),
        ) {
            let dib = crate::clipboard::png_to_dibv5(&std::fs::read(&path).unwrap()).unwrap();
            let header = u32::from_le_bytes(dib[0..4].try_into().unwrap()) as usize;
            let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
            let signed_height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
            let height = signed_height.abs();
            let stride = usize::try_from(width).unwrap() * 4;
            let rows = usize::try_from(height).unwrap();
            let mut pixels = dib[header..header + stride * rows].to_vec();
            if signed_height > 0 {
                for y in 0..rows / 2 {
                    let (head, tail) = pixels.split_at_mut((rows - 1 - y) * stride);
                    head[y * stride..(y + 1) * stride].swap_with_slice(&mut tail[..stride]);
                }
            }
            let started = std::time::Instant::now();
            let mut early_ms = 0;
            let regions = detect(
                &pixels,
                RECT {
                    right: width,
                    bottom: height,
                    ..RECT::default()
                },
                Region {
                    x: 0,
                    y: 0,
                    w: width,
                    h: height,
                },
                |_| {
                    early_ms = started.elapsed().as_millis();
                },
            );
            let images: Vec<_> = regions
                .iter()
                .filter(|r| {
                    r.w > 450 && r.w < 600 && r.h > 280 && r.h < 440 && r.x > 450 && r.x < 550
                })
                .collect();
            println!(
                "fixture={} size={}x{} early_ms={} elapsed_ms={} image_candidates={images:?}",
                path.file_name().unwrap().to_string_lossy(),
                width,
                height,
                early_ms,
                started.elapsed().as_millis()
            );
            assert!(
                images.len() >= 2,
                "Expected separate chat image bounds, candidates={regions:?}"
            );
        }
        unsafe { CoUninitialize() };
    }
}
