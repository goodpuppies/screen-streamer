//! Validate CPU video chunks before packing the helper's tightly packed BGRA wire format.
#[derive(Clone, Copy)]
pub(super) enum PixelOrder {
    Bgra,
    Bgrx,
    Rgba,
    Rgbx,
}

pub(super) fn pack_bgra(
    data: &[u8],
    offset: usize,
    size: usize,
    stride: i32,
    width: u32,
    height: u32,
    order: PixelOrder,
) -> Result<Vec<u8>, &'static str> {
    let row = (width as usize).checked_mul(4).ok_or("row size overflow")?;
    let output_size = row
        .checked_mul(height as usize)
        .ok_or("frame size overflow")?;
    if output_size == 0 || output_size > 256 * 1024 * 1024 {
        return Err("invalid frame dimensions");
    }
    let stride = usize::try_from(stride).map_err(|_| "negative stride is unsupported")?;
    if stride < row {
        return Err("stride is shorter than a pixel row");
    }
    if data.is_empty() {
        return Err("empty mapping");
    }
    // SPA defines chunk.offset modulo maxsize and clamps chunk.size to maxsize.
    let offset = offset % data.len();
    let size = size.min(data.len());
    let needed = (height as usize - 1)
        .checked_mul(stride)
        .and_then(|n| n.checked_add(row))
        .ok_or("chunk size overflow")?;
    if needed > size
        || offset
            .checked_add(needed)
            .filter(|&n| n <= data.len())
            .is_none()
    {
        return Err("truncated or wrapped video chunk");
    }
    let mut output = Vec::with_capacity(output_size);
    for y in 0..height as usize {
        let start = offset + y * stride;
        for px in data[start..start + row].chunks_exact(4) {
            let (b, g, r) = match order {
                PixelOrder::Bgra | PixelOrder::Bgrx => (px[0], px[1], px[2]),
                PixelOrder::Rgba | PixelOrder::Rgbx => (px[2], px[1], px[0]),
            };
            let alpha = match order {
                PixelOrder::Bgrx | PixelOrder::Rgbx => 255,
                _ => px[3],
            };
            output.extend_from_slice(&[b, g, r, alpha]);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_offset_padding_and_unused_tail() {
        let bytes = [99, 99, 1, 2, 3, 0, 88, 88, 88, 88, 4, 5, 6, 0, 77, 77];
        assert_eq!(
            pack_bgra(&bytes, 2, 12, 8, 1, 2, PixelOrder::Bgrx).unwrap(),
            [1, 2, 3, 255, 4, 5, 6, 255]
        );
    }
    #[test]
    fn swaps_red_blue_and_preserves_alpha() {
        assert_eq!(
            pack_bgra(&[3, 2, 1, 42], 0, 4, 4, 1, 1, PixelOrder::Rgba).unwrap(),
            [1, 2, 3, 42]
        );
        assert_eq!(
            pack_bgra(&[3, 2, 1, 0], 0, 4, 4, 1, 1, PixelOrder::Rgbx).unwrap(),
            [1, 2, 3, 255]
        );
        assert_eq!(
            pack_bgra(&[1, 2, 3, 42], 0, 4, 4, 1, 1, PixelOrder::Bgra).unwrap(),
            [1, 2, 3, 42]
        );
    }
    #[test]
    fn rejects_incomplete_rows_and_invalid_geometry() {
        for (offset, size, stride, width, height) in [
            (0, 3, 4, 1, 1),
            (2, 4, 4, 1, 1),
            (0, 4, 2, 1, 1),
            (0, 4, -4, 1, 1),
            (0, 4, 4, 0, 1),
            (0, 4, 4, 1, 0),
            (0, 4, 4, u32::MAX, u32::MAX),
        ] {
            assert!(pack_bgra(
                &[0; 4],
                offset,
                size,
                stride,
                width,
                height,
                PixelOrder::Bgrx
            )
            .is_err());
        }
    }
    #[test]
    fn accepts_spa_offset_modulo_mapping_size() {
        assert_eq!(
            pack_bgra(&[1, 2, 3, 0], 4, 4, 4, 1, 1, PixelOrder::Bgrx).unwrap(),
            [1, 2, 3, 255]
        );
    }
}
