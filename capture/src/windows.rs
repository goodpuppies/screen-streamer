//! Windows Graphics Capture (WGC) of one monitor, on the caller's D3D11 device.
//!
//! WGC captures one monitor per session, so sharing one display captures only that display. Frames
//! arrive as BGRA textures on the device the session was created with; an encoder on that device
//! reads them directly (a video processor converting into its input surface), then drops the
//! [`Frame`] to give the texture back to the pool.
//!
//! The calling thread joins the multithreaded COM apartment. The free-threaded frame pool signals
//! new frames from a system thread; [`Capture::next_frame`] waits for that signal and takes the
//! newest frame, so a slow consumer skips frames instead of queuing them.
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::time::Duration;

use windows::core::{factory, Error, IInspectable, Interface, Result, BOOL};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow,
    ShowWindow, GWL_EXSTYLE, SW_RESTORE, WS_EX_TOOLWINDOW,
};

/// `MONITORINFOF_PRIMARY`.
const PRIMARY: u32 = 1;
/// Frames the pool holds: one being read, one arriving.
const POOL_FRAMES: i32 = 2;

/// A monitor, in physical pixels when the process is per-monitor DPI aware (otherwise Windows
/// scales the rectangle; the captured frames are physical either way).
#[derive(Clone, Debug)]
pub struct Monitor {
    /// The `HMONITOR`, valid while the monitor stays connected.
    pub handle: isize,
    /// The adapter output's device name, e.g. `\\.\DISPLAY1`.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Make monitor rectangles physical pixels, as the captured frames are. Once per process, before
/// [`monitors`]; a process that already chose its DPI awareness keeps it.
pub fn use_physical_pixels() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The connected monitors, in the order Windows enumerates them.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let handles = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        handles.push(monitor);
        true.into()
    }
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut handles as *mut _ as isize));
    }
    handles
        .into_iter()
        .filter_map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let ok = unsafe { GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO) };
            if !ok.as_bool() {
                return None;
            }
            let rect = info.monitorInfo.rcMonitor;
            let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            Some(Monitor {
                handle: handle.0 as isize,
                name: String::from_utf16_lossy(&info.szDevice[..end]),
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
                primary: info.monitorInfo.dwFlags & PRIMARY != 0,
            })
        })
        .collect()
}

/// A top-level visible user window (not cloaked, not a tool window).
#[derive(Clone, Debug)]
pub struct Window {
    pub handle: isize,
    pub title: String,
    pub app: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
    pub active: bool,
}

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned()
    }
}

/// Top-level user windows in Z-order (filtering out invisible, cloaked, and tool windows).
pub fn windows() -> Vec<Window> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        true.into()
    }
    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut handles as *mut _ as isize));
    }
    let foreground = unsafe { GetForegroundWindow() };
    handles
        .into_iter()
        .filter_map(|hwnd| unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return None;
            }
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
                return None;
            }
            let mut cloaked: u32 = 0;
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0
            {
                return None;
            }
            let title_len = GetWindowTextLengthW(hwnd);
            if title_len <= 0 {
                return None;
            }
            let mut title_buf = vec![0u16; (title_len as usize) + 1];
            let copied = GetWindowTextW(hwnd, &mut title_buf);
            if copied <= 0 {
                return None;
            }
            let title = String::from_utf16_lossy(&title_buf[..copied as usize]);
            let minimized = IsIconic(hwnd).as_bool();
            let mut rect = RECT::default();
            if DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_err()
                && GetWindowRect(hwnd, &mut rect).is_err()
            {
                return None;
            }
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            if !minimized && (width < 16 || height < 16) {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            Some(Window {
                handle: hwnd.0 as isize,
                title,
                app: process_name(pid),
                pid,
                x: rect.left,
                y: rect.top,
                width,
                height,
                minimized,
                active: hwnd == foreground,
            })
        })
        .collect()
}

/// Restore (if minimized) and bring a window to the foreground.
pub fn focus_window(handle: isize) -> bool {
    unsafe {
        let hwnd = HWND(handle as *mut core::ffi::c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// One captured frame. Its texture belongs to the capture's pool: read it (on the capture's
/// device), then drop the frame so the pool can reuse it.
pub struct Frame {
    frame: Direct3D11CaptureFrame,
    /// BGRA (`DXGI_FORMAT_B8G8R8A8_UNORM`). It can be larger than `width`×`height` right after
    /// the monitor changes size; the picture is its top-left corner.
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// When the frame was composed, in 100 ns units of the QPC clock
    /// (`QueryPerformanceCounter` time, as `SystemRelativeTime` gives it).
    pub time_100ns: i64,
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

/// A running capture of one monitor.
pub struct Capture {
    device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    arrived: Receiver<()>,
    token: i64,
}

impl Capture {
    /// Capture `monitor` (a [`Monitor::handle`]) onto `device`, with or without the cursor
    /// drawn in. Windows 10 draws a yellow border around a captured monitor; Windows 11 lets the
    /// capture turn it off, which this asks for.
    pub fn start(device: &ID3D11Device, monitor: isize, cursor: bool) -> Result<Self> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor(HMONITOR(monitor as *mut core::ffi::c_void))? };
        Self::start_item(device, item, cursor)
    }

    /// Capture one window (`hwnd` from [`Window::handle`]) onto `device` via WGC.
    pub fn start_window(device: &ID3D11Device, hwnd: isize, cursor: bool) -> Result<Self> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(HWND(hwnd as *mut core::ffi::c_void))? };
        Self::start_item(device, item, cursor)
    }

    fn start_item(device: &ID3D11Device, item: GraphicsCaptureItem, cursor: bool) -> Result<Self> {
        let dxgi: IDXGIDevice = device.cast()?;
        let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            POOL_FRAMES,
            size,
        )?;
        let (signal, arrived) = sync_channel::<()>(1);
        let token = pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            // A pending signal already covers this frame.
            let _ = signal.try_send(());
            Ok(())
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        // Both are newer than the first WGC release; a system without them keeps the defaults.
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture()?;
        Ok(Self { device: winrt_device, pool, session, size, arrived, token })
    }

    /// The newest frame, waiting up to `timeout` for one. `None` when the screen did not change
    /// in that time: WGC sends frames only when something on the monitor is redrawn.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.arrived.recv_timeout(timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => return Err(Error::from(E_FAIL)),
        }
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = latest.replace(frame) {
                let _ = older.Close();
            }
        }
        let Some(frame) = latest else { return Ok(None) };
        let content = frame.ContentSize()?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            // The monitor changed resolution: later frames come at the new size.
            self.size = content;
            self.pool.Recreate(
                &self.device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                POOL_FRAMES,
                content,
            )?;
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let time_100ns = frame.SystemRelativeTime()?.Duration;
        Ok(Some(Frame {
            frame,
            texture,
            width: content.Width.max(0) as u32,
            height: content.Height.max(0) as u32,
            time_100ns,
        }))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
