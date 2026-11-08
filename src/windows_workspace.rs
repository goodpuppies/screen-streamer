//! Windows Graphics Capture only captures one monitor per session, so a multi-monitor desktop runs
//! one capture per monitor and stitches them into a single frame.

use crate::workspace::{blit_bgra, bounding_rect, Rect};
use crate::{bgra_frame, capture_options};
use scap::capturer::Capturer;
use scap::Target;
use std::error::Error;
use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
struct WinRect {
  left: i32,
  top: i32,
  right: i32,
  bottom: i32,
}

#[repr(C)]
#[derive(Default)]
struct MonitorInfo {
  size: u32,
  monitor: WinRect,
  work: WinRect,
  flags: u32,
}

#[link(name = "user32")]
extern "system" {
  fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
  fn SetProcessDpiAwarenessContext(context: isize) -> i32;
}

const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

/// Physical pixels are what the captures deliver, and what monitor rectangles must be in.
pub fn use_physical_pixels() {
  // Fails harmlessly when something already chose the process's awareness.
  unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

fn monitor_rect(handle: isize) -> Option<Rect> {
  let mut info = MonitorInfo {
    size: std::mem::size_of::<MonitorInfo>() as u32,
    ..Default::default()
  };
  if unsafe { GetMonitorInfoW(handle as *mut c_void, &mut info) } == 0 {
    return None;
  }
  Some(Rect {
    x: info.monitor.left,
    y: info.monitor.top,
    width: (info.monitor.right - info.monitor.left) as u32,
    height: (info.monitor.bottom - info.monitor.top) as u32,
  })
}

pub struct MonitorTarget {
  target: Target,
  rect: Rect,
}

// An HMONITOR is a process-wide identifier, not tied to the thread that obtained it.
unsafe impl Send for MonitorTarget {}

pub fn monitor_targets() -> Vec<MonitorTarget> {
  scap::get_all_targets()
    .into_iter()
    .filter_map(|target| {
      let Target::Display(display) = &target else {
        return None;
      };
      let rect = monitor_rect(display.raw_handle.0 as isize)?;
      Some(MonitorTarget { target, rect })
    })
    .collect()
}

pub fn first_target(mut monitors: Vec<MonitorTarget>) -> Option<Target> {
  (!monitors.is_empty()).then(|| monitors.remove(0).target)
}

struct MonitorFrame {
  data: Vec<u8>,
  width: u32,
  height: u32,
}

#[derive(Default)]
struct Latest {
  frames: Vec<Option<MonitorFrame>>,
  error: Option<String>,
}

#[derive(Default)]
struct Shared {
  latest: Mutex<Latest>,
  changed: Condvar,
}

impl Shared {
  fn fail(&self, message: String) {
    self.latest.lock().unwrap().error.get_or_insert(message);
    self.changed.notify_all();
  }
}

pub struct StitchedCapture {
  shared: Arc<Shared>,
  offsets: Vec<(u32, u32)>,
  canvas: Vec<u8>,
  width: u32,
  height: u32,
  min_frame_interval: Duration,
  last_frame_at: Instant,
}

impl StitchedCapture {
  /// Starts one capture per monitor; frames then flow from [Self::next_frame].
  pub fn start(
    monitors: Vec<MonitorTarget>,
    fps: u64,
    token_path: &Path,
  ) -> Result<Self, Box<dyn Error>> {
    let rects: Vec<Rect> = monitors.iter().map(|monitor| monitor.rect).collect();
    let bounds = bounding_rect(&rects).ok_or("no monitors to capture")?;
    let offsets = rects
      .iter()
      .map(|rect| ((rect.x - bounds.x) as u32, (rect.y - bounds.y) as u32))
      .collect();
    let shared = Arc::new(Shared::default());
    shared.latest.lock().unwrap().frames = rects.iter().map(|_| None).collect();

    for (index, monitor) in monitors.into_iter().enumerate() {
      let shared = Arc::clone(&shared);
      let token_path = token_path.to_path_buf();
      thread::spawn(move || run_monitor(index, monitor, fps, &token_path, &shared));
    }

    Ok(Self {
      shared,
      offsets,
      canvas: vec![0; bounds.width as usize * bounds.height as usize * 4],
      width: bounds.width,
      height: bounds.height,
      min_frame_interval: Duration::from_secs_f64(1.0 / fps as f64),
      last_frame_at: Instant::now(),
    })
  }

  pub fn frame_size(&self) -> [u32; 2] {
    [self.width, self.height]
  }

  /// Blocks until a monitor has changed, then returns the updated desktop. Each monitor keeps its
  /// last image on the canvas, since Windows only delivers frames for monitors that changed.
  pub fn next_frame(&mut self) -> Result<(Vec<u8>, u32, u32), String> {
    // Several monitors updating together should cost one frame, not one each.
    if let Some(wait) = self
      .min_frame_interval
      .checked_sub(self.last_frame_at.elapsed())
    {
      sleep(wait);
    }
    let updates: Vec<(usize, MonitorFrame)> = {
      let mut latest = self
        .shared
        .changed
        .wait_while(self.shared.latest.lock().unwrap(), |latest| {
          latest.error.is_none() && latest.frames.iter().all(Option::is_none)
        })
        .unwrap();
      if let Some(error) = latest.error.take() {
        return Err(error);
      }
      latest
        .frames
        .iter_mut()
        .enumerate()
        .filter_map(|(index, frame)| Some((index, frame.take()?)))
        .collect()
    };
    for (index, frame) in updates {
      blit_bgra(
        &mut self.canvas,
        self.width,
        self.height,
        &frame.data,
        frame.width,
        frame.height,
        self.offsets[index],
      );
    }
    self.last_frame_at = Instant::now();
    Ok((self.canvas.clone(), self.width, self.height))
  }
}

fn run_monitor(index: usize, monitor: MonitorTarget, fps: u64, token_path: &Path, shared: &Shared) {
  let options = capture_options(Some(monitor.target), fps, token_path);
  let mut capturer = match Capturer::build(options) {
    Ok(capturer) => capturer,
    Err(error) => return shared.fail(format!("monitor {index}: {error}")),
  };
  capturer.start_capture();
  loop {
    match capturer.get_next_frame() {
      Ok(frame) => {
        let Some((data, width, height)) = bgra_frame(frame) else {
          continue;
        };
        shared.latest.lock().unwrap().frames[index] = Some(MonitorFrame {
          data,
          width,
          height,
        });
        shared.changed.notify_all();
      }
      Err(error) => return shared.fail(format!("monitor {index}: {error}")),
    }
  }
}
