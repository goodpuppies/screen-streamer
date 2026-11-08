//! Places separately captured monitors on one BGRA canvas covering the whole desktop, the same
//! Full Workspace frame the Linux portal delivers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
}

/// The smallest rectangle containing every monitor.
pub fn bounding_rect(rects: &[Rect]) -> Option<Rect> {
  let left = rects.iter().map(|r| r.x).min()?;
  let top = rects.iter().map(|r| r.y).min()?;
  let right = rects.iter().map(|r| r.x + r.width as i32).max()?;
  let bottom = rects.iter().map(|r| r.y + r.height as i32).max()?;
  Some(Rect {
    x: left,
    y: top,
    width: (right - left) as u32,
    height: (bottom - top) as u32,
  })
}

/// Copies a tightly packed BGRA image onto the canvas at `at`, clipped to the canvas. A monitor
/// whose frame no longer matches its rectangle (a mode change mid-capture) is clipped, not fatal.
pub fn blit_bgra(
  canvas: &mut [u8],
  canvas_width: u32,
  canvas_height: u32,
  image: &[u8],
  image_width: u32,
  image_height: u32,
  at: (u32, u32),
) {
  let (at_x, at_y) = (at.0 as usize, at.1 as usize);
  let canvas_stride = canvas_width as usize * 4;
  let image_stride = image_width as usize * 4;
  if at.0 >= canvas_width || image_stride == 0 {
    return;
  }
  let row_bytes = image_stride.min(canvas_stride - at_x * 4);
  let rows = image_height.min(canvas_height.saturating_sub(at.1)) as usize;
  for row in 0..rows {
    let source_start = row * image_stride;
    let target_start = (at_y + row) * canvas_stride + at_x * 4;
    let (Some(source), Some(target)) = (
      image.get(source_start..source_start + row_bytes),
      canvas.get_mut(target_start..target_start + row_bytes),
    ) else {
      return;
    };
    target.copy_from_slice(source);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
    Rect {
      x,
      y,
      width,
      height,
    }
  }

  #[test]
  fn bounds_include_monitors_left_of_and_above_the_primary() {
    let rects = [rect(0, 0, 2560, 1440), rect(-1920, -200, 1920, 1080)];
    assert_eq!(bounding_rect(&rects), Some(rect(-1920, -200, 4480, 1640)));
    assert_eq!(bounding_rect(&[]), None);
  }

  #[test]
  fn blit_places_each_row_at_the_offset() {
    let mut canvas = vec![0_u8; 3 * 2 * 4];
    let image = [1, 1, 1, 1, 2, 2, 2, 2];
    blit_bgra(&mut canvas, 3, 2, &image, 2, 1, (1, 1));
    assert_eq!(&canvas[..12], &[0; 12]);
    assert_eq!(&canvas[12..], &[0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2]);
  }

  #[test]
  fn blit_clips_images_larger_than_the_canvas_and_short_buffers() {
    let mut canvas = vec![0_u8; 2 * 2 * 4];
    blit_bgra(&mut canvas, 2, 2, &[7; 3 * 3 * 4], 3, 3, (1, 1));
    assert_eq!(canvas, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 7, 7, 7]);

    let mut canvas = vec![0_u8; 2 * 2 * 4];
    blit_bgra(&mut canvas, 2, 2, &[9; 8], 2, 2, (0, 0));
    assert_eq!(canvas, [9, 9, 9, 9, 9, 9, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0]);
  }
}
