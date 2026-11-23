//! KDE Plasma 6 (Wayland) window enumeration, focus, and zero-popup per-window PipeWire capture.
//!
//! Uses KWin's native Wayland protocols:
//! - `org_kde_plasma_window_management`: list windows (`uuid`, title, app_id, geometry, state)
//!   and focus/unminimize a window.
//! - `zkde_screencast_unstable_v1`: request a direct PipeWire node ID for any window by UUID
//!   with no XDG portal dialog.
//!
//! Requires a one-time KWin permission desktop file in `/usr/share/applications/` granting
//! `X-KDE-Wayland-Interfaces=org_kde_plasma_window_management,zkde_screencast_unstable_v1`
//! to this binary (see `resources/linux/petplay-screen-streamer.kwin.desktop`).

use std::collections::HashMap;
use std::sync::{
  atomic::{AtomicBool, Ordering},
  mpsc, Arc,
};
use std::thread;

use wayland_client::{
  protocol::{wl_output, wl_registry},
  Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_plasma::plasma_window_management::client::{
  org_kde_plasma_window::{self, OrgKdePlasmaWindow},
  org_kde_plasma_window_management::{self, OrgKdePlasmaWindowManagement},
};
use wayland_protocols_plasma::screencast::v1::client::{
  zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1 as ZkdeScreencastStreamV1},
  zkde_screencast_unstable_v1::{self, ZkdeScreencastUnstableV1 as ZkdeScreencastV1},
};

const STATE_ACTIVE: u32 = 0x1;
const STATE_MINIMIZED: u32 = 0x2;
const STATE_SKIP_TASKBAR: u32 = 0x1000;
const STATE_SKIP_SWITCHER: u32 = 0x40000;

#[derive(Clone, Debug, Default)]
pub struct KdeWindow {
  pub id: String,
  pub title: String,
  pub app: String,
  pub pid: u32,
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  pub minimized: bool,
  pub active: bool,
  flags: u32,
  unmapped: bool,
}

impl KdeWindow {
  fn is_user_window(&self) -> bool {
    !self.unmapped
      && !self.id.is_empty()
      && !self.title.is_empty()
      && (self.flags & STATE_SKIP_TASKBAR) == 0
      && (self.flags & STATE_SKIP_SWITCHER) == 0
      && self.width > 0
      && self.height > 0
  }

  pub fn to_json(&self) -> String {
    format!(
      "{{\"id\":\"{}\",\"title\":\"{}\",\"app\":\"{}\",\"pid\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"minimized\":{},\"active\":{}}}",
      json_escape(&self.id),
      json_escape(&self.title),
      json_escape(&self.app),
      self.pid,
      self.x,
      self.y,
      self.width,
      self.height,
      self.minimized,
      self.active,
    )
  }
}

fn json_escape(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for ch in s.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
      c => out.push(c),
    }
  }
  out
}

struct WindowListState {
  mgmt: Option<OrgKdePlasmaWindowManagement>,
  windows: HashMap<String, KdeWindow>,
  proxies: HashMap<String, OrgKdePlasmaWindow>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WindowListState {
  fn event(
    state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      if interface == OrgKdePlasmaWindowManagement::interface().name {
        let bind_ver = version.min(18);
        state.mgmt =
          Some(registry.bind::<OrgKdePlasmaWindowManagement, _, _>(name, bind_ver, qh, ()));
      }
    }
  }
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for WindowListState {
  fn event(
    state: &mut Self,
    mgmt: &OrgKdePlasmaWindowManagement,
    event: org_kde_plasma_window_management::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    if let org_kde_plasma_window_management::Event::WindowWithUuid { uuid, .. } = event {
      let proxy = mgmt.get_window_by_uuid(uuid.clone(), qh, uuid.clone());
      state.windows.entry(uuid.clone()).or_insert_with(|| KdeWindow {
        id: uuid.clone(),
        ..Default::default()
      });
      state.proxies.insert(uuid, proxy);
    }
  }
}

impl Dispatch<OrgKdePlasmaWindow, String> for WindowListState {
  fn event(
    state: &mut Self,
    _: &OrgKdePlasmaWindow,
    event: org_kde_plasma_window::Event,
    uuid: &String,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    let entry = state.windows.entry(uuid.clone()).or_insert_with(|| KdeWindow {
      id: uuid.clone(),
      ..Default::default()
    });
    match event {
      org_kde_plasma_window::Event::TitleChanged { title } => entry.title = title,
      org_kde_plasma_window::Event::AppIdChanged { app_id } => entry.app = app_id,
      org_kde_plasma_window::Event::PidChanged { pid } => entry.pid = pid,
      org_kde_plasma_window::Event::StateChanged { flags } => {
        entry.flags = flags;
        entry.minimized = (flags & STATE_MINIMIZED) != 0;
        entry.active = (flags & STATE_ACTIVE) != 0;
      }
      org_kde_plasma_window::Event::Geometry {
        x,
        y,
        width,
        height,
      } => {
        entry.x = x;
        entry.y = y;
        entry.width = width;
        entry.height = height;
      }
      org_kde_plasma_window::Event::Unmapped => entry.unmapped = true,
      _ => {}
    }
  }
}

fn connect_window_mgmt() -> Result<(Connection, EventQueue<WindowListState>, WindowListState), String> {
  let conn = Connection::connect_to_env()
    .map_err(|e| format!("Wayland connect failed (is WAYLAND_DISPLAY set?): {e}"))?;
  let mut queue: EventQueue<WindowListState> = conn.new_event_queue();
  let qh = queue.handle();
  let _registry = conn.display().get_registry(&qh, ());
  let mut state = WindowListState {
    mgmt: None,
    windows: HashMap::new(),
    proxies: HashMap::new(),
  };
  // Roundtrip 1: receive registry globals & bind org_kde_plasma_window_management.
  queue
    .roundtrip(&mut state)
    .map_err(|e| format!("Wayland registry roundtrip failed: {e}"))?;
  if state.mgmt.is_none() {
    return Err(
      "org_kde_plasma_window_management not available (install petplay-screen-streamer.kwin.desktop in /usr/share/applications/)"
        .into(),
    );
  }
  // Roundtrip 2: receive WindowWithUuid events and request each window object.
  queue
    .roundtrip(&mut state)
    .map_err(|e| format!("Wayland window list roundtrip failed: {e}"))?;
  // Roundtrip 3: receive TitleChanged / AppIdChanged / Geometry / StateChanged for each window.
  queue
    .roundtrip(&mut state)
    .map_err(|e| format!("Wayland window state roundtrip failed: {e}"))?;
  Ok((conn, queue, state))
}

pub fn list_windows() -> Result<Vec<KdeWindow>, String> {
  let (_conn, _queue, state) = connect_window_mgmt()?;
  let mut windows: Vec<KdeWindow> = state
    .windows
    .into_values()
    .filter(|w| w.is_user_window())
    .collect();
  windows.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.title.cmp(&b.title)));
  Ok(windows)
}

pub fn focus_window(uuid: &str) -> Result<(), String> {
  let (_conn, mut queue, mut state) = connect_window_mgmt()?;
  let proxy = state
    .proxies
    .get(uuid)
    .ok_or_else(|| format!("window {uuid} not found"))?;
  // Clear minimized bit and set active bit.
  proxy.set_state(STATE_ACTIVE | STATE_MINIMIZED, STATE_ACTIVE);
  queue
    .roundtrip(&mut state)
    .map_err(|e| format!("Wayland focus roundtrip failed: {e}"))?;
  Ok(())
}

// -----------------------------------------------------------------------------------------------
// zkde_screencast_unstable_v1: zero-popup per-window PipeWire node

struct ScreencastState {
  screencast: Option<ZkdeScreencastV1>,
  node_tx: Option<mpsc::Sender<Result<u32, String>>>,
  closed: Arc<AtomicBool>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ScreencastState {
  fn event(
    state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      if interface == ZkdeScreencastV1::interface().name {
        let bind_ver = version.min(4);
        state.screencast = Some(registry.bind::<ZkdeScreencastV1, _, _>(name, bind_ver, qh, ()));
      }
    }
  }
}

impl Dispatch<ZkdeScreencastV1, ()> for ScreencastState {
  fn event(
    _: &mut Self,
    _: &ZkdeScreencastV1,
    _: zkde_screencast_unstable_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ZkdeScreencastStreamV1, ()> for ScreencastState {
  fn event(
    state: &mut Self,
    _: &ZkdeScreencastStreamV1,
    event: zkde_screencast_stream_unstable_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zkde_screencast_stream_unstable_v1::Event::Created { node } => {
        if let Some(tx) = state.node_tx.take() {
          let _ = tx.send(Ok(node));
        }
      }
      zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
        state.closed.store(true, Ordering::Relaxed);
        if let Some(tx) = state.node_tx.take() {
          let _ = tx.send(Err(format!("KWin screencast failed: {error}")));
        }
      }
      zkde_screencast_stream_unstable_v1::Event::Closed => {
        state.closed.store(true, Ordering::Relaxed);
      }
      _ => {}
    }
  }
}

/// Keeps the KWin screencast Wayland object alive; `is_closed()` flips true if the window closes.
pub struct KdeWindowStream {
  pub node_id: u32,
  closed: Arc<AtomicBool>,
}

impl KdeWindowStream {
  pub fn is_closed(&self) -> bool {
    self.closed.load(Ordering::Relaxed)
  }

  pub fn close(&self) {
    self.closed.store(true, Ordering::Relaxed);
  }
}

struct OutputScreencastState {
  screencast: Option<ZkdeScreencastV1>,
  outputs: Vec<(wl_output::WlOutput, Option<String>)>,
  node_tx: Option<mpsc::Sender<Result<u32, String>>>,
  closed: Arc<AtomicBool>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for OutputScreencastState {
  fn event(
    state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _: &(),
    _: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    if let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    {
      if interface == ZkdeScreencastV1::interface().name {
        let bind_ver = version.min(4);
        state.screencast = Some(registry.bind::<ZkdeScreencastV1, _, _>(name, bind_ver, qh, ()));
      } else if interface == wl_output::WlOutput::interface().name {
        let idx = state.outputs.len();
        let out = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, idx);
        state.outputs.push((out, None));
      }
    }
  }
}

impl Dispatch<wl_output::WlOutput, usize> for OutputScreencastState {
  fn event(
    state: &mut Self,
    _: &wl_output::WlOutput,
    event: wl_output::Event,
    idx: &usize,
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    if let wl_output::Event::Name { name } = event {
      if let Some(entry) = state.outputs.get_mut(*idx) {
        entry.1 = Some(name);
      }
    }
  }
}

impl Dispatch<ZkdeScreencastV1, ()> for OutputScreencastState {
  fn event(
    _: &mut Self,
    _: &ZkdeScreencastV1,
    _: zkde_screencast_unstable_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<ZkdeScreencastStreamV1, ()> for OutputScreencastState {
  fn event(
    state: &mut Self,
    _: &ZkdeScreencastStreamV1,
    event: zkde_screencast_stream_unstable_v1::Event,
    _: &(),
    _: &Connection,
    _: &QueueHandle<Self>,
  ) {
    match event {
      zkde_screencast_stream_unstable_v1::Event::Created { node } => {
        if let Some(tx) = state.node_tx.take() {
          let _ = tx.send(Ok(node));
        }
      }
      zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
        state.closed.store(true, Ordering::Relaxed);
        if let Some(tx) = state.node_tx.take() {
          let _ = tx.send(Err(format!("KWin output screencast failed: {error}")));
        }
      }
      zkde_screencast_stream_unstable_v1::Event::Closed => {
        state.closed.store(true, Ordering::Relaxed);
      }
      _ => {}
    }
  }
}

/// Open a direct zero-popup KWin PipeWire screencast for a single monitor output by name (e.g. "DP-1").
pub fn open_output_stream(output_name: &str, show_cursor: bool) -> Result<KdeWindowStream, String> {
  let target_name = output_name.to_owned();
  let (tx, rx) = mpsc::channel::<Result<u32, String>>();
  let closed = Arc::new(AtomicBool::new(false));
  let closed_bg = closed.clone();

  thread::spawn(move || {
    let res = (|| -> Result<(), String> {
      let conn = Connection::connect_to_env()
        .map_err(|e| format!("Wayland connect failed: {e}"))?;
      let mut queue: EventQueue<OutputScreencastState> = conn.new_event_queue();
      let qh = queue.handle();
      let _registry = conn.display().get_registry(&qh, ());
      let mut state = OutputScreencastState {
        screencast: None,
        outputs: Vec::new(),
        node_tx: Some(tx.clone()),
        closed: closed_bg.clone(),
      };
      // Two roundtrips: first binds wl_output globals, second receives wl_output::name events.
      queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland registry roundtrip failed: {e}"))?;
      queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland output roundtrip failed: {e}"))?;
      let screencast = state.screencast.as_ref().ok_or_else(|| {
        "zkde_screencast_unstable_v1 not available (install petplay-screen-streamer.kwin.desktop in /usr/share/applications/)"
          .to_owned()
      })?;
      let (output, _) = state
        .outputs
        .iter()
        .find(|(_, name)| name.as_deref() == Some(target_name.as_str()))
        .ok_or_else(|| {
          let available: Vec<_> = state
            .outputs
            .iter()
            .filter_map(|(_, n)| n.as_deref())
            .collect();
          format!("KDE output '{target_name}' not found (available: {available:?})")
        })?;
      let pointer = if show_cursor {
        zkde_screencast_unstable_v1::Pointer::Embedded as u32
      } else {
        zkde_screencast_unstable_v1::Pointer::Hidden as u32
      };
      let _stream = screencast.stream_output(output, pointer, &qh, ());
      conn.flush().map_err(|e| format!("Wayland flush failed: {e}"))?;

      while !closed_bg.load(Ordering::Relaxed) {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
          closed_bg.store(true, Ordering::Relaxed);
          return Err(format!("Wayland output screencast dispatch ended: {e}"));
        }
      }
      Ok(())
    })();
    if let Err(err) = res {
      closed_bg.store(true, Ordering::Relaxed);
      let _ = tx.send(Err(err));
    }
  });

  let node_id = rx
    .recv()
    .map_err(|_| "Wayland output screencast thread exited before returning a node".to_owned())??;
  Ok(KdeWindowStream { node_id, closed })
}

pub fn open_window_stream(uuid: &str, show_cursor: bool) -> Result<KdeWindowStream, String> {
  let uuid = uuid.to_owned();
  let (tx, rx) = mpsc::channel::<Result<u32, String>>();
  let closed = Arc::new(AtomicBool::new(false));
  let closed_bg = closed.clone();

  thread::spawn(move || {
    let res = (|| -> Result<(), String> {
      let conn = Connection::connect_to_env()
        .map_err(|e| format!("Wayland connect failed: {e}"))?;
      let mut queue: EventQueue<ScreencastState> = conn.new_event_queue();
      let qh = queue.handle();
      let _registry = conn.display().get_registry(&qh, ());
      let mut state = ScreencastState {
        screencast: None,
        node_tx: Some(tx.clone()),
        closed: closed_bg.clone(),
      };
      queue
        .roundtrip(&mut state)
        .map_err(|e| format!("Wayland registry roundtrip failed: {e}"))?;
      let screencast = state.screencast.as_ref().ok_or_else(|| {
        "zkde_screencast_unstable_v1 not available (install petplay-screen-streamer.kwin.desktop in /usr/share/applications/)"
          .to_owned()
      })?;
      let pointer = if show_cursor {
        zkde_screencast_unstable_v1::Pointer::Embedded as u32
      } else {
        zkde_screencast_unstable_v1::Pointer::Hidden as u32
      };
      let _stream = screencast.stream_window(uuid, pointer, &qh, ());
      conn.flush().map_err(|e| format!("Wayland flush failed: {e}"))?;

      while !closed_bg.load(Ordering::Relaxed) {
        if let Err(e) = queue.blocking_dispatch(&mut state) {
          closed_bg.store(true, Ordering::Relaxed);
          return Err(format!("Wayland screencast dispatch ended: {e}"));
        }
      }
      Ok(())
    })();
    if let Err(err) = res {
      closed_bg.store(true, Ordering::Relaxed);
      let _ = tx.send(Err(err));
    }
  });

  let node_id = rx
    .recv()
    .map_err(|_| "Wayland screencast thread exited before returning a node".to_owned())??;
  Ok(KdeWindowStream { node_id, closed })
}
