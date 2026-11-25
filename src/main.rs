//! `screen-streamer`: single-process StageForge native actor (`protocol: 1` NDJSON over stdio)
//! for desktop/output/window capture and Linux uinput injection.
//!
//! Only `--version` is accepted as a CLI probe (for build/smoke scripts); otherwise this binary
//! speaks StageForge control messages over `stdin`/`stdout` (logs go to `stderr`) and streams
//! pixel data over dedicated transports (TCP frame sockets and/or DMA-BUF Unix seqpacket sockets).

use scap::capturer::{Capturer, Options};
use scap::frame::{Frame, VideoFrame};
use serde_json::{json, Value};
use stageforge_actor::{Actor, ActorHandle};
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{
  atomic::{AtomicBool, Ordering},
  mpsc, Arc, Mutex,
};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

mod control_input;

#[cfg(target_os = "linux")]
mod kde_window;
#[cfg(target_os = "linux")]
mod media_producer;

#[cfg(target_os = "windows")]
mod windows_workspace;
#[cfg(any(target_os = "windows", test))]
mod workspace;

const CHUNK_SIZE: usize = 256 * 1024;
const USE_RGBA_CONVERSION: bool = false;

#[cfg(target_os = "linux")]
fn linux_keycode(scan_hex: &str) -> Option<i32> {
  let scan = u16::from_str_radix(scan_hex.trim_start_matches("0x"), 16).ok()?;
  Some(match scan {
    0xE01C => 96,
    0xE01D => 97,
    0xE035 => 98,
    0xE037 => 99,
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
    0xE05D => 127,
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
            resolution: 0,
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
            resolution: 0,
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
      eprintln!("input: uinput keyboard and mouse ready");

      while let Ok(line) = rx.recv() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        match parts.as_slice() {
          ["M", x, y] => {
            if let (Ok(nx), Ok(ny)) = (x.parse::<f64>(), y.parse::<f64>()) {
              let ax = (nx.clamp(0.0, 1.0) * ABS_EXTENT as f64).round() as i32;
              let ay = (ny.clamp(0.0, 1.0) * ABS_EXTENT as f64).round() as i32;
              let _ = mouse.write(&[
                event(EV_ABS, 0, ax),
                event(EV_ABS, 1, ay),
                event(EV_SYN, 0, 0),
              ]);
            }
          }
          ["B", button, state] => {
            let code = match *button {
              "left" => Some(0x110),
              "right" => Some(0x111),
              "middle" => Some(0x112),
              _ => None,
            };
            if let (Some(code), Ok(val)) = (code, state.parse::<i32>()) {
              let _ = mouse.write(&[event(EV_KEY, code, val), event(EV_SYN, 0, 0)]);
            }
          }
          ["W", delta] => {
            if let Ok(steps) = delta.parse::<i32>() {
              let _ = mouse.write(&[
                event(EV_REL, 8, -steps),
                event(EV_REL, 11, -steps * 120),
                event(EV_SYN, 0, 0),
              ]);
            }
          }
          ["K", scan, state] => {
            if let (Some(code), Ok(val)) = (linux_keycode(scan), state.parse::<i32>()) {
              let _ = keyboard.write(&[event(EV_KEY, code as u16, val), event(EV_SYN, 0, 0)]);
            }
          }
          _ => {}
        }
      }
      Ok(())
    })();
    if let Err(err) = result {
      eprintln!("input: uinput disabled: {err}");
    }
  });
  tx
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
  let mut rgba = Vec::with_capacity(bgra.len());
  for chunk in bgra.chunks_exact(4) {
    rgba.push(chunk[2]);
    rgba.push(chunk[1]);
    rgba.push(chunk[0]);
    rgba.push(chunk[3]);
  }
  rgba
}

pub(crate) fn bgra_frame(frame: Frame) -> Option<(Vec<u8>, u32, u32)> {
  let (data, width, height) = match frame {
    Frame::Video(VideoFrame::BGRA(frame)) => (frame.data, frame.width, frame.height),
    Frame::Video(VideoFrame::BGRx(frame)) => (frame.data, frame.width, frame.height),
    Frame::Video(VideoFrame::XBGR(frame)) => (frame.data, frame.width, frame.height),
    Frame::Video(VideoFrame::RGBx(frame)) => {
      (bgra_to_rgba(&frame.data), frame.width, frame.height)
    }
    _ => return None,
  };
  Some((data, width.max(1) as u32, height.max(1) as u32))
}

pub(crate) fn capture_options(
  target: Option<scap::Target>,
  fps: u64,
  token_path: &Path,
) -> Options {
  #[cfg(target_os = "linux")]
  let options = Options {
    fps: fps as u32,
    target,
    show_cursor: true,
    show_highlight: false,
    excluded_targets: None,
    output_type: scap::frame::FrameType::BGRAFrame,
    output_resolution: scap::capturer::Resolution::Captured,
    crop_area: None,
    restore_token: read_restore_token(token_path),
    ..Default::default()
  };
  #[cfg(not(target_os = "linux"))]
  let options = {
    let _ = token_path;
    Options {
      fps: fps as u32,
      target,
      show_cursor: true,
      show_highlight: false,
      excluded_targets: None,
      output_type: scap::frame::FrameType::BGRAFrame,
      output_resolution: scap::capturer::Resolution::Captured,
      crop_area: None,
      ..Default::default()
    }
  };
  options
}

enum Next {
  Frame(Option<(Vec<u8>, u32, u32)>),
  Idle,
}

enum Capture {
  Single(Capturer),
  #[cfg(target_os = "windows")]
  Stitched(windows_workspace::StitchedCapture),
}

impl Capture {
  fn frame_size(&mut self) -> [u32; 2] {
    match self {
      Capture::Single(c) => c.get_output_frame_size(),
      #[cfg(target_os = "windows")]
      Capture::Stitched(w) => w.frame_size(),
    }
  }

  fn start(&mut self) {
    match self {
      Capture::Single(c) => c.start_capture(),
      #[cfg(target_os = "windows")]
      Capture::Stitched(_) => {}
    }
  }

  fn stop(&mut self) {
    match self {
      Capture::Single(c) => c.stop_capture(),
      #[cfg(target_os = "windows")]
      Capture::Stitched(_) => {}
    }
  }

  fn next_frame_within(&mut self, timeout: Duration) -> Result<Next, String> {
    match self {
      Capture::Single(c) => match c.get_next_frame_within(timeout) {
        Ok(Some(frame)) => Ok(Next::Frame(bgra_frame(frame))),
        Ok(None) => Ok(Next::Idle),
        Err(e) => Err(e.to_string()),
      },
      #[cfg(target_os = "windows")]
      Capture::Stitched(w) => w
        .next_frame()
        .map(|f| Next::Frame(Some(f)))
        .map_err(|e| e.to_string()),
    }
  }
}

fn open_workspace_capture(fps: u64, token_path: &Path) -> Result<Capture, String> {
  #[cfg(target_os = "windows")]
  {
    let monitors = windows_workspace::monitor_targets();
    if monitors.len() > 1 {
      let stitched = windows_workspace::StitchedCapture::start(monitors, fps, token_path)
        .map_err(|e| format!("StitchedCapture::start failed: {e}"))?;
      return Ok(Capture::Stitched(stitched));
    }
    let opts = capture_options(windows_workspace::first_target(monitors), fps, token_path);
    let capturer = Capturer::build(opts).map_err(|e| format!("Capturer::build failed: {e:?}"))?;
    return Ok(Capture::Single(capturer));
  }
  #[cfg(not(target_os = "windows"))]
  {
    let opts = capture_options(None, fps, token_path);
    let capturer = Capturer::build(opts).map_err(|e| format!("Capturer::build failed: {e:?}"))?;
    #[cfg(target_os = "linux")]
    if let Some(token) = capturer.restore_token() {
      let _ = save_restore_token(token_path, token);
    }
    Ok(Capture::Single(capturer))
  }
}

enum CaptureTarget {
  Workspace,
  #[cfg(target_os = "linux")]
  Output(String),
  #[cfg(target_os = "linux")]
  Window(String),
}

struct CaptureJob {
  stop: Arc<AtomicBool>,
  #[cfg(target_os = "linux")]
  kde_stream: Option<kde_window::KdeWindowStream>,
}

impl CaptureJob {
  fn cancel(&self) {
    self.stop.store(true, Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    if let Some(s) = &self.kde_stream {
      s.close();
    }
  }
}

struct CaptureConfig {
  id: String,
  target: CaptureTarget,
  frame_port: Option<u16>,
  media_socket: Option<String>,
  fps: u64,
  token_path: PathBuf,
  subscriber: String,
}

fn spawn_capture(
  cfg: CaptureConfig,
  handle: ActorHandle,
  jobs: Arc<Mutex<HashMap<String, CaptureJob>>>,
) -> Result<(), String> {
  if !scap::is_supported() {
    return Err("capture platform not supported".to_owned());
  }
  if !scap::has_permission() && !scap::request_permission() {
    return Err("capture permission denied".to_owned());
  }

  let stop = Arc::new(AtomicBool::new(false));
  #[cfg(target_os = "linux")]
  let kde_stream = match &cfg.target {
    CaptureTarget::Window(uuid) => Some(kde_window::open_window_stream(uuid, true)?),
    CaptureTarget::Output(name) => Some(kde_window::open_output_stream(name, true)?),
    CaptureTarget::Workspace => None,
  };

  #[cfg(target_os = "linux")]
  let mut capture = if let Some(stream) = &kde_stream {
    let mut opts = capture_options(None, cfg.fps, &cfg.token_path);
    opts.pipewire_node_id = Some(stream.node_id);
    Capturer::build(opts)
      .map(Capture::Single)
      .map_err(|e| format!("Capturer::build(pipewire) failed: {e:?}"))?
  } else {
    open_workspace_capture(cfg.fps, &cfg.token_path)?
  };

  // Register job so STOP_CAPTURE / SHUTDOWN can cancel it cleanly.
  {
    let mut map = jobs.lock().unwrap();
    if let Some(prev) = map.remove(&cfg.id) {
      prev.cancel();
    }
    map.insert(
      cfg.id.clone(),
      CaptureJob {
        stop: Arc::clone(&stop),
        #[cfg(target_os = "linux")]
        kde_stream: None,
      },
    );
  }

  let stop_bg = Arc::clone(&stop);
  let jobs_bg = Arc::clone(&jobs);
  thread::spawn(move || {
    let reason = (|| -> Result<(), String> {
      // On Windows, scap::Capturer holds !Send WinRT/COM pointers and must be created on the
      // capture thread that drives it.
      #[cfg(not(target_os = "linux"))]
      let mut capture = open_workspace_capture(cfg.fps, &cfg.token_path)?;
      let mut socket = match cfg.frame_port {
        Some(port) => Some(
          TcpStream::connect(("127.0.0.1", port))
            .map_err(|e| format!("frame socket connect 127.0.0.1:{port}: {e}"))?,
        ),
        None => None,
      };
      let [mut width, mut height] = capture.frame_size();
      capture.start();
      let _ = handle.post(
        &cfg.subscriber,
        "CAPTURE_STARTED",
        json!({ "id": cfg.id, "width": width, "height": height }),
      );

      #[cfg(target_os = "linux")]
      let mut media: Option<media_producer::MediaProducer> = None;
      #[cfg(target_os = "linux")]
      let mut media_retry_at: Option<Instant> = None;
      #[cfg(target_os = "linux")]
      let mut media_last: Option<Vec<u8>> = None;
      #[cfg(target_os = "linux")]
      let mut repeats_left = 0u32;
      let poll_timeout = Duration::from_millis((1000 / cfg.fps.max(1)).clamp(16, 100));

      while !stop_bg.load(Ordering::Relaxed) {
        #[cfg(target_os = "linux")]
        if kde_stream.as_ref().is_some_and(|s| s.is_closed()) {
          break;
        }
        match capture.next_frame_within(poll_timeout) {
          Ok(Next::Idle) => {
            #[cfg(target_os = "linux")]
            if repeats_left > 0 {
              if let (Some(producer), Some(last)) = (media.as_mut(), media_last.as_deref()) {
                repeats_left -= 1;
                if producer.publish(last, width, height).is_err() {
                  media = None;
                  media_retry_at = Some(Instant::now() + Duration::from_secs(1));
                }
              }
            }
          }
          Ok(Next::Frame(Some((bgra_data, fw, fh)))) => {
            width = fw;
            height = fh;
            let frame_data = if USE_RGBA_CONVERSION {
              bgra_to_rgba(&bgra_data)
            } else {
              bgra_data
            };

            #[cfg(target_os = "linux")]
            if let Some(media_sock) = cfg.media_socket.as_deref() {
              let even_w = (width & !1).max(16);
              let even_h = (height & !1).max(16);
              // Recreate GBM/DMA-BUF pool if the captured window resized!
              if media
                .as_ref()
                .is_some_and(|p| p.geometry() != (even_w, even_h))
              {
                media = None;
                media_retry_at = None;
              }
              if media.is_none() && media_retry_at.map_or(true, |at| Instant::now() >= at) {
                match media_producer::MediaProducer::connect(
                  Path::new(media_sock),
                  Path::new("/dev/dri/renderD128"),
                  even_w,
                  even_h,
                ) {
                  Ok(p) => {
                    media = Some(p);
                    media_retry_at = None;
                  }
                  Err(_) => {
                    media_retry_at = Some(Instant::now() + Duration::from_secs(1));
                  }
                }
              }
              if let Some(producer) = media.as_mut() {
                if producer.publish(&frame_data, width, height).is_ok() {
                  repeats_left = 4;
                } else {
                  media = None;
                  media_retry_at = Some(Instant::now() + Duration::from_secs(1));
                }
              }
            }

            if let Some(sock) = socket.as_mut() {
              let total_size = frame_data.len() as u32;
              let num_chunks = ((total_size as usize + CHUNK_SIZE - 1) / CHUNK_SIZE) as u32;
              let mut metadata = [0u8; 16];
              metadata[0..4].copy_from_slice(&width.to_le_bytes());
              metadata[4..8].copy_from_slice(&height.to_le_bytes());
              metadata[8..12].copy_from_slice(&total_size.to_le_bytes());
              metadata[12..16].copy_from_slice(&num_chunks.to_le_bytes());
              if sock.write_all(&metadata).is_err() {
                break;
              }
              let mut ok = true;
              for chunk in frame_data.chunks(CHUNK_SIZE) {
                let len = chunk.len() as u32;
                if sock.write_all(&len.to_le_bytes()).is_err() || sock.write_all(chunk).is_err() {
                  ok = false;
                  break;
                }
              }
              if !ok {
                break;
              }
            }

            #[cfg(target_os = "linux")]
            if media.is_some() {
              media_last = Some(frame_data);
            }
          }
          Ok(Next::Frame(None)) => sleep(Duration::from_millis(1)),
          Err(err) => {
            capture.stop();
            return Err(err);
          }
        }
      }

      capture.stop();
      #[cfg(target_os = "linux")]
      if let Some(s) = kde_stream {
        s.close();
      }
      Ok(())
    })();

    jobs_bg.lock().unwrap().remove(&cfg.id);
    let _ = handle.post(
      &cfg.subscriber,
      "CAPTURE_ENDED",
      json!({
        "id": cfg.id,
        "error": reason.err(),
      }),
    );
  });

  Ok(())
}

fn main() -> io::Result<()> {
  if std::env::args().any(|a| a == "--version" || a == "-V") {
    println!("screen-streamer {}", env!("CARGO_PKG_VERSION"));
    return Ok(());
  }

  let mut actor = Actor::new("screenStreamer");
  let jobs: Arc<Mutex<HashMap<String, CaptureJob>>> = Arc::new(Mutex::new(HashMap::new()));
  #[cfg(target_os = "linux")]
  let uinput: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));

  actor.on("LISTWINDOWS", |_ctx, _payload| {
    #[cfg(target_os = "linux")]
    {
      let wins = kde_window::list_windows()?;
      let arr: Vec<Value> = wins
        .into_iter()
        .map(|w| {
          json!({
            "id": w.id,
            "title": w.title,
            "app": w.app,
            "pid": w.pid,
            "x": w.x,
            "y": w.y,
            "width": w.width,
            "height": w.height,
            "minimized": w.minimized,
            "active": w.active,
          })
        })
        .collect();
      Ok(Value::Array(arr))
    }
    #[cfg(not(target_os = "linux"))]
    Ok(Value::Array(Vec::new()))
  });

  actor.on("FOCUSWINDOW", |_ctx, payload| {
    #[cfg(target_os = "linux")]
    {
      let id = payload
        .get("window")
        .and_then(Value::as_str)
        .ok_or_else(|| "FOCUSWINDOW needs {window}".to_owned())?;
      kde_window::focus_window(id)?;
      Ok(Value::Bool(true))
    }
    #[cfg(not(target_os = "linux"))]
    {
      let _ = payload;
      Ok(Value::Bool(true))
    }
  });

  {
    let jobs = Arc::clone(&jobs);
    actor.on("START_CAPTURE", move |ctx, payload| {
      let id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
      let fps = payload
        .get("fps")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 240);
      let frame_port = payload
        .get("framePort")
        .and_then(Value::as_u64)
        .map(|p| p as u16);
      let media_socket = payload
        .get("mediaSocket")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
      let token_path = payload
        .get("tokenPath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("screen-streamer-capture.token"));
      let subscriber = payload
        .get("subscriber")
        .and_then(Value::as_str)
        .unwrap_or(ctx.sender)
        .to_owned();

      let target_obj = payload.get("target");
      let kind = target_obj
        .and_then(|t| t.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("workspace");
      let target = match kind {
        "workspace" => CaptureTarget::Workspace,
        #[cfg(target_os = "linux")]
        "output" => {
          let name = target_obj
            .and_then(|t| t.get("output"))
            .and_then(Value::as_str)
            .ok_or_else(|| "output capture needs target.output".to_owned())?;
          CaptureTarget::Output(name.to_owned())
        }
        #[cfg(target_os = "linux")]
        "window" => {
          let win = target_obj
            .and_then(|t| t.get("window"))
            .and_then(Value::as_str)
            .ok_or_else(|| "window capture needs target.window".to_owned())?;
          CaptureTarget::Window(win.to_owned())
        }
        other => return Err(format!("unsupported capture target kind: {other}")),
      };

      spawn_capture(
        CaptureConfig {
          id: id.clone(),
          target,
          frame_port,
          media_socket,
          fps,
          token_path,
          subscriber,
        },
        ctx.handle.clone(),
        Arc::clone(&jobs),
      )?;
      Ok(json!({ "id": id }))
    });
  }

  {
    let jobs = Arc::clone(&jobs);
    actor.on("STOP_CAPTURE", move |_ctx, payload| {
      let id = payload.get("id").and_then(Value::as_str);
      let mut map = jobs.lock().unwrap();
      if let Some(id) = id {
        if let Some(job) = map.remove(id) {
          job.cancel();
        }
      } else {
        for (_, job) in map.drain() {
          job.cancel();
        }
      }
      Ok(Value::Bool(true))
    });
  }

  #[cfg(target_os = "linux")]
  {
    let uinput = Arc::clone(&uinput);
    actor.on("INPUT", move |_ctx, payload| {
      let line = payload
        .as_str()
        .or_else(|| payload.get("line").and_then(Value::as_str))
        .ok_or_else(|| "INPUT needs a control line string".to_owned())?
        .to_owned();
      let mut guard = uinput.lock().unwrap();
      let tx = guard.get_or_insert_with(spawn_uinput);
      let _ = tx.send(line);
      Ok(Value::Bool(true))
    });
  }

  {
    let jobs = Arc::clone(&jobs);
    actor.on_shutdown(move || {
      for (_, job) in jobs.lock().unwrap().drain() {
        job.cancel();
      }
    });
  }

  actor.run_stdio()
}
