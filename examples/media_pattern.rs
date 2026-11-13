//! Publish a moving synthetic pattern to a running media worker.
//!
//! This exercises the whole producer contract — linear GBM allocation, DMA-BUF export,
//! the 64-byte descriptor, slot acknowledgement and the GPU encoder's strict import —
//! without a capture portal and without putting any of the screen's real content into
//! an encoded stream. The capture backend is the only part it does not cover.
//!
//! ```sh
//! petplay-media /tmp/media.sock - --layout mono --fps 60 &
//! cargo run --release --example media_pattern -- /tmp/media.sock 1920 1080 120
//! ```

#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("the media producer is Linux-only");
  std::process::exit(1);
}

#[path = "../src/media_producer.rs"]
#[cfg(target_os = "linux")]
mod media_producer;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
  let arguments: Vec<String> = std::env::args().skip(1).collect();
  let [socket, width, height, frames] = arguments.as_slice() else {
    return Err("usage: media_pattern SOCKET WIDTH HEIGHT FRAMES".into());
  };
  let (width, height): (u32, u32) = (width.parse()?, height.parse()?);
  let frames: u32 = frames.parse()?;
  let mut producer = media_producer::MediaProducer::connect(
    std::path::Path::new(socket),
    std::path::Path::new("/dev/dri/renderD128"),
    width & !1,
    height & !1,
  )?;
  let (width, height) = producer.geometry();
  let mut image = vec![0u8; width as usize * height as usize * 4];
  for frame in 0..frames {
    // A bar sweeping left to right over a gradient, so a receiver can tell a fresh
    // frame from a repeated one.
    let bar = (frame as usize * 17) % width as usize;
    for y in 0..height as usize {
      for x in 0..width as usize {
        let at = (y * width as usize + x) * 4;
        let near = x.abs_diff(bar) < 24;
        image[at] = if near { 255 } else { (x * 255 / width as usize) as u8 };
        image[at + 1] = if near { 32 } else { (y * 255 / height as usize) as u8 };
        image[at + 2] = if near { 32 } else { 64 };
        image[at + 3] = 255;
      }
    }
    // Real capture drops a frame when every slot is still encoding. This tool waits
    // instead, so the published count is exactly what was asked for.
    while !producer.publish(&image, width, height)? {
      std::thread::sleep(std::time::Duration::from_millis(1));
    }
  }
  println!(
    "PATTERN_DONE published={} dropped={}",
    producer.published(),
    producer.dropped()
  );
  Ok(())
}
