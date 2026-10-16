use scap::capturer::{Capturer, Options};
use scap::frame::{Frame, VideoFrame};
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};
use std::{env, str::FromStr};

const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
const USE_RGBA_CONVERSION: bool = false; // Toggle BGRA to RGBA conversion

#[cfg(target_os = "linux")]
fn linux_keycode(scan_hex: &str) -> Option<i32> {
  let scan = u16::from_str_radix(scan_hex.trim_start_matches("0x"), 16).ok()?;
  Some(match scan {
    0xE01D => 97,
    0xE038 => 100,
    0xE047 => 102,
    0xE048 => 103,
    0xE049 => 104,
    0xE04B => 105,
    0xE04D => 106,
    0xE04F => 107,
    0xE050 => 108,
    0xE051 => 109,
    0xE052 => 110,
    0xE053 => 111,
    0xE05B => 125,
    0xE05C => 126,
    value if value <= 0x7f => value as i32,
    _ => return None,
  })
}

fn read_restore_token(path: &Path) -> Option<String> {
  std::fs::read_to_string(path)
    .ok()
    .map(|token| token.trim().to_owned())
    .filter(|token| !token.is_empty())
}

fn save_restore_token(path: &Path, token: &str) -> io::Result<()> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  let temporary = path.with_extension("tmp");
  std::fs::write(&temporary, token)?;
  std::fs::rename(temporary, path)
}

#[cfg(target_os = "linux")]
fn spawn_uinput() -> mpsc::Sender<String> {
  use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, Key, RelativeAxis,
    UInputHandle,
  };
  use libc::{input_event, timeval};
  use std::fs::File;
  use std::mem::transmute;

  const EV_SYN: u16 = 0;
  const EV_KEY: u16 = 1;
  const EV_REL: u16 = 2;
  const EV_ABS: u16 = 3;
  const ABS_EXTENT: i32 = 32768;

  fn event(type_: u16, code: u16, value: i32) -> input_event {
    let mut time = timeval {
      tv_sec: 0,
      tv_usec: 0,
    };
    unsafe { libc::gettimeofday(&raw mut time, std::ptr::null_mut()) };
    input_event {
      time,
      type_,
      code,
      value,
    }
  }

  let (tx, rx) = mpsc::channel::<String>();
  thread::spawn(move || {
    let result = (|| -> io::Result<()> {
      let keyboard = UInputHandle::new(File::create("/dev/uinput")?);
      keyboard.set_evbit(EventKind::Key)?;
      for code in 0_u16..=255 {
        let key: Key = unsafe { transmute(code) };
        let _ = keyboard.set_keybit(key);
      }
      keyboard.create(
        &InputId {
          bustype: 0x03,
          vendor: 0x4711,
          product: 0x0831,
          version: 1,
        },
        b"PetPlay Keyboard\0",
        0,
        &[],
      )?;

      let mouse = UInputHandle::new(File::create("/dev/uinput")?);
      mouse.set_evbit(EventKind::Absolute)?;
      mouse.set_evbit(EventKind::Relative)?;
      mouse.set_evbit(EventKind::Key)?;
      mouse.set_absbit(AbsoluteAxis::X)?;
      mouse.set_absbit(AbsoluteAxis::Y)?;
      mouse.set_relbit(RelativeAxis::Wheel)?;
      mouse.set_relbit(RelativeAxis::WheelHiRes)?;
      for code in [0x110_u16, 0x111, 0x112] {
        mouse.set_keybit(unsafe { transmute(code) })?;
      }
      let axes = [
        AbsoluteInfoSetup {
          axis: AbsoluteAxis::X,
          info: AbsoluteInfo {
            value: 0,
            minimum: 0,
            maximum: ABS_EXTENT,
            fuzz: 0,
            flat: 0,
            resolution: 10,
          },
        },
        AbsoluteInfoSetup {
          axis: AbsoluteAxis::Y,
          info: AbsoluteInfo {
            value: 0,
            minimum: 0,
            maximum: ABS_EXTENT,
            fuzz: 0,
            flat: 0,
            resolution: 10,
          },
        },
      ];
      mouse.create(
        &InputId {
          bustype: 0x03,
          vendor: 0x4711,
          product: 0x0832,
          version: 1,
        },
        b"PetPlay Mouse\0",
        0,
        &axes,
      )?;
      eprintln!("uinput ready (absolute workspace pointer + keyboard)");

      while let Ok(line) = rx.recv() {
        let parts: Vec<_> = line.trim().split(',').collect();
        match parts.as_slice() {
          ["M", x, y] => {
            if let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) {
              mouse.write(&[
                event(
                  EV_ABS,
                  AbsoluteAxis::X as u16,
                  (x.clamp(0.0, 1.0) * f64::from(ABS_EXTENT)) as i32,
                ),
                event(
                  EV_ABS,
                  AbsoluteAxis::Y as u16,
                  (y.clamp(0.0, 1.0) * f64::from(ABS_EXTENT)) as i32,
                ),
                event(EV_SYN, 0, 0),
              ])?;
            }
          }
          ["B", button, state] => {
            let button = match *button {
              "left" => 0x110_u16,
              "right" => 0x111_u16,
              "middle" => 0x112_u16,
              _ => continue,
            };
            mouse.write(&[
              event(EV_KEY, button, i32::from(*state == "1")),
              event(EV_SYN, 0, 0),
            ])?;
          }
          ["W", delta] => {
            if let Ok(delta) = delta.parse::<i32>() {
              mouse.write(&[
                event(EV_REL, RelativeAxis::Wheel as u16, -delta),
                event(EV_REL, RelativeAxis::WheelHiRes as u16, -delta * 120),
                event(EV_SYN, 0, 0),
              ])?;
            }
          }
          ["K", scan, state] => {
            if let Some(code) = linux_keycode(scan) {
              keyboard.write(&[
                event(EV_KEY, code as u16, i32::from(*state == "1")),
                event(EV_SYN, 0, 0),
              ])?;
            }
          }
          _ => {}
        }
      }
      Ok(())
    })();
    if let Err(error) = result {
      eprintln!("uinput failed: {error}");
    }
  });
  tx
}

fn argument<T: FromStr>(name: &str, fallback: T) -> T {
  let prefix = format!("--{name}=");
  env::args()
    .find_map(|arg| {
      arg
        .strip_prefix(&prefix)
        .and_then(|value| value.parse().ok())
    })
    .unwrap_or(fallback)
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
  let mut rgba = Vec::with_capacity(bgra.len());
  for chunk in bgra.chunks(4) {
    rgba.push(chunk[2]); // R
    rgba.push(chunk[1]); // G
    rgba.push(chunk[0]); // B
    rgba.push(chunk[3]); // A
  }
  rgba
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  if env::args().any(|arg| arg == "--version") {
    println!("screen-streamer {}", env!("CARGO_PKG_VERSION"));
    return Ok(());
  }
  let target_fps = argument("fps", 60_u64).clamp(1, 240);
  let port = argument("port", 12345_u16);
  let capture_token_path = PathBuf::from(argument(
    "capture-token-path",
    String::from("screen-streamer-capture.token"),
  ));
  // Check if the platform is supported
  if !scap::is_supported() {
    println!("❌ Platform not supported");
    return Ok(());
  }

  // Check if we have permission to capture screen
  if !scap::has_permission() {
    println!("❌ Permission not granted. Requesting permission...");
    if !scap::request_permission() {
      println!("❌ Permission denied");
      return Ok(());
    }
  }

  println!("✅ Platform supported and permission granted");

  // Linux chooses the target interactively through the ScreenCast portal.
  #[cfg(target_os = "linux")]
  let target = None;
  #[cfg(not(target_os = "linux"))]
  let target = scap::get_all_targets()
    .into_iter()
    .find(|t| matches!(t, scap::Target::Display(_)));

  // Create Options for screen capture
  let options = Options {
    fps: target_fps as u32,
    target,
    show_cursor: true,
    show_highlight: false,
    excluded_targets: None,
    output_type: scap::frame::FrameType::BGRAFrame,
    output_resolution: scap::capturer::Resolution::Captured,
    crop_area: None, // Use full display area
    restore_token: read_restore_token(&capture_token_path),
    ..Default::default()
  };

  // Create capturer and get frame size
  let mut capturer = Capturer::build(options).expect("Failed to create capturer");
  if let Some(token) = capturer.restore_token() {
    save_restore_token(&capture_token_path, token)?;
  }
  let [mut width, mut height] = capturer.get_output_frame_size();

  // Connect to TCP server
  println!("\n🔌 Connecting to 127.0.0.1:{port}");
  let mut socket = TcpStream::connect(("127.0.0.1", port))?;
  println!("✅ Connected to server");

  // Calculate buffer sizes based on resolution
  println!("⚙️ Capture target: {width}x{height} @ {target_fps}fps (max)");

  // Pre-allocate buffers
  let mut current_frame: Option<Vec<u8>> = None;
  let mut is_sending = false;
  let mut frame_count = 0;
  let mut dropped_frames = 0;
  let mut last_fps_print = Instant::now();

  #[cfg(target_os = "linux")]
  let input_tx = spawn_uinput();

  // Forward line-oriented control commands from PetPlay.
  let (tx, rx) = mpsc::channel::<String>();
  thread::spawn(move || loop {
    let mut input = String::new();
    if io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
      break;
    }
    if tx.send(input).is_err() {
      break;
    }
  });

  // Start capture
  capturer.start_capture();
  println!("\n🎥 Started capture. Press Enter to stop...");
  println!("\nStreaming... ");

  loop {
    let frame_start = Instant::now();

    while let Ok(command) = rx.try_recv() {
      #[cfg(target_os = "linux")]
      if input_tx.send(command).is_err() {
        break;
      }
    }

    // If we're not currently sending, try to get a new frame
    if !is_sending {
      match capturer.get_next_frame() {
        Ok(frame) => {
          // Get the raw bytes from the frame
          let (bgra_data, frame_width, frame_height) = match frame {
            Frame::Video(VideoFrame::BGRA(frame)) => (frame.data, frame.width, frame.height),
            Frame::Video(VideoFrame::BGRx(frame)) => (frame.data, frame.width, frame.height),
            Frame::Video(VideoFrame::XBGR(frame)) => (frame.data, frame.width, frame.height),
            Frame::Video(VideoFrame::RGBx(frame)) => {
              (bgra_to_rgba(&frame.data), frame.width, frame.height)
            }
            _ => {
              println!("\n❌ Unexpected frame format");
              sleep(Duration::from_millis(1));
              continue;
            }
          };
          width = frame_width.max(1) as u32;
          height = frame_height.max(1) as u32;

          // Convert and store the frame
          current_frame = Some(if USE_RGBA_CONVERSION {
            bgra_to_rgba(&bgra_data)
          } else {
            bgra_data.to_vec()
          });
          is_sending = true;
        }
        Err(e) => {
          println!("\n❌ Error getting frame: {:?}", e);
          sleep(Duration::from_millis(1));
          continue;
        }
      }
    }

    // Try to send the current frame
    if let Some(frame_data) = current_frame.take() {
      // Send frame metadata
      let total_size = frame_data.len() as u32;
      let num_chunks = ((total_size as usize + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32;
      let metadata = [
        width.to_le_bytes(),
        height.to_le_bytes(),
        total_size.to_le_bytes(),
        num_chunks.to_le_bytes(),
      ]
      .concat();

      if let Err(e) = socket.write_all(&metadata) {
        println!("\n❌ Connection error on metadata: {:?}", e);
        break;
      }

      // Send the frame data in chunks
      let mut send_error = false;

      for chunk in frame_data.chunks(CHUNK_SIZE) {
        // Send chunk size first
        let chunk_size = chunk.len() as u32;
        if let Err(e) = socket.write_all(&chunk_size.to_le_bytes()) {
          println!("\n❌ Connection error on chunk size: {:?}", e);
          send_error = true;
          break;
        }

        // Then send chunk data
        if let Err(e) = socket.write_all(chunk) {
          println!("\n❌ Connection error on chunk data: {:?}", e);
          send_error = true;
          break;
        }
      }

      if send_error {
        break;
      }

      frame_count += 1;
      is_sending = false;

      // Print FPS and stats every second
      if last_fps_print.elapsed().as_secs() >= 1 {
        let fps = frame_count as f64 / last_fps_print.elapsed().as_secs_f64();
        let latency = frame_start.elapsed().as_millis() as f64;
        let drop_rate = (dropped_frames as f64 / (frame_count + dropped_frames) as f64) * 100.0;

        print!(
          "\r🎬 FPS: {:.1} | Latency: {:.1}ms | Dropped: {}/{} ({:.1}%)    ",
          fps,
          latency,
          dropped_frames,
          frame_count + dropped_frames,
          drop_rate
        );
        io::stdout().flush().unwrap();

        frame_count = 0;
        dropped_frames = 0;
        last_fps_print = Instant::now();
      }
    }
  }

  // Stop Capture
  capturer.stop_capture();
  println!("\n👋 Capture stopped");
  Ok(())
}
