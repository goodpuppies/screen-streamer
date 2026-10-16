use std::{
    cell::Cell,
    os::fd::OwnedFd,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU8},
        mpsc::{self, sync_channel, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        self,
        buffer::DataType,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{Pod, Property},
        utils::{Direction, SpaTypes},
    },
    stream::{Stream, StreamRc, StreamState},
};

use crate::{
    capturer::Options,
    frame::{BGRAFrame, Frame, VideoFrame},
};

use self::{error::LinCapError, portal::ScreenCastPortal};

mod error;
mod pixels;
mod portal;
use pixels::{pack_bgra, PixelOrder};

static CAPTURER_STATE: AtomicU8 = AtomicU8::new(0);
static STREAM_STATE_CHANGED_TO_ERROR: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct ListenerUserData {
    pub tx: mpsc::Sender<Frame>,
    pub format: spa::param::video::VideoInfoRaw,
    warned_bad_buffer: bool,
}

fn param_changed_callback(
    _stream: &Stream,
    user_data: &mut ListenerUserData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    if let Err(error) = user_data.format.parse(param) {
        eprintln!("capture: invalid PipeWire video format: {error}");
        STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn state_changed_callback(
    _stream: &Stream,
    _user_data: &mut ListenerUserData,
    _old: StreamState,
    new: StreamState,
) {
    match new {
        StreamState::Error(e) => {
            eprintln!("pipewire: State changed to error({e})");
            STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        _ => {}
    }
}

fn process_callback(stream: &Stream, user_data: &mut ListenerUserData) {
    // The RAII buffer always requeues, including empty/invalid/error paths.
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let result = (|| -> Result<Vec<u8>, &'static str> {
        let datas = buffer.datas_mut();
        if datas.len() != 1 {
            return Err("expected one packed video plane");
        }
        let plane = &mut datas[0];
        if plane.type_() != DataType::MemFd && plane.type_() != DataType::MemPtr {
            return Err("non-CPU buffer received; DMA-BUF import is not enabled");
        }
        if plane.as_raw().chunk.is_null() {
            return Err("missing video chunk");
        }
        let chunk = unsafe { *plane.as_raw().chunk };
        if chunk.flags != 0 {
            return Err("empty or corrupted video chunk");
        }
        let order = match user_data.format.format() {
            VideoFormat::BGRA => PixelOrder::Bgra,
            VideoFormat::BGRx => PixelOrder::Bgrx,
            VideoFormat::RGBA => PixelOrder::Rgba,
            VideoFormat::RGBx => PixelOrder::Rgbx,
            _ => return Err("unsupported negotiated pixel format"),
        };
        let size = user_data.format.size();
        let data = plane.data().ok_or("video buffer is not mapped")?;
        pack_bgra(
            data,
            chunk.offset as usize,
            chunk.size as usize,
            chunk.stride,
            size.width,
            size.height,
            order,
        )
    })();
    match result {
        Ok(data) => {
            let size = user_data.format.size();
            let _ = user_data.tx.send(Frame::Video(VideoFrame::BGRA(BGRAFrame {
                display_time: std::time::SystemTime::now(),
                width: size.width as i32,
                height: size.height as i32,
                data,
            })));
        }
        Err(error) if !user_data.warned_bad_buffer => {
            user_data.warned_bad_buffer = true;
            eprintln!("capture: dropping invalid PipeWire buffer: {error}");
        }
        Err(_) => {}
    }
}

// TODO: Format negotiation
fn pipewire_capturer(
    options: Options,
    tx: mpsc::Sender<Frame>,
    ready_sender: &SyncSender<bool>,
    stream_id: u32,
    remote: OwnedFd,
) -> Result<(), LinCapError> {
    pw::init();

    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(remote, None)?;

    // The portal can return while permissions/global announcements are still in flight.
    // Wait until its source is visible on this restricted remote before targeting it.
    let registry = core.get_registry_rc()?;
    let source_ready = Rc::new(Cell::new(false));
    let ready = source_ready.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.id == stream_id && global.type_ == pw::types::ObjectType::Node {
                ready.set(true);
            }
        })
        .register();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !source_ready.get() {
        if Instant::now() >= deadline {
            return Err(LinCapError::new(format!(
                "Portal PipeWire node {stream_id} did not become available"
            )));
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }

    let user_data = ListenerUserData {
        tx,
        format: Default::default(),
        warned_bad_buffer: false,
    };

    let stream = StreamRc::new(
        core,
        "scap",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data.clone())
        .state_changed(state_changed_callback)
        .param_changed(param_changed_callback)
        .process(process_callback)
        .register()?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                // Default
                width: 128,
                height: 128,
            },
            pw::spa::utils::Rectangle {
                // Min
                width: 1,
                height: 1,
            },
            pw::spa::utils::Rectangle {
                // Max
                width: 16384,
                height: 16384,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction {
                num: options.fps,
                denom: 1
            }
        ),
    );

    // Request CPU-mappable buffers. Do not accidentally negotiate DMA-BUF then dereference it.
    let buffers_obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamBuffers,
        ParamType::Buffers,
        Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            pw::spa::pod::Value::Choice(pw::spa::pod::ChoiceValue::Int(pw::spa::utils::Choice(
                pw::spa::utils::ChoiceFlags::empty(),
                pw::spa::utils::ChoiceEnum::Flags {
                    default: (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr),
                    flags: vec![],
                }
            )))
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let buffers_values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(buffers_obj),
    )?
    .0
    .into_inner();

    let mut params = [
        pw::spa::pod::Pod::from_bytes(&values).unwrap(),
        pw::spa::pod::Pod::from_bytes(&buffers_values).unwrap(),
    ];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    ready_sender.send(true)?;

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }

    let pw_loop = mainloop.loop_();

    // User has called Capturer::start() and we start the main loop
    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 1
        && /* If the stream state got changed to `Error`, we exit. TODO: tell user that we exited */
          !STREAM_STATE_CHANGED_TO_ERROR.load(std::sync::atomic::Ordering::Relaxed)
    {
        pw_loop.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }

    Ok(())
}

pub struct LinuxCapturer {
    capturer_join_handle: Option<JoinHandle<Result<(), LinCapError>>>,
    // The pipewire stream is deleted when the connection is dropped.
    // That's why we keep it alive
    _connection: dbus::blocking::Connection,
    restore_token: Option<String>,
}

impl LinuxCapturer {
    // TODO: Error handling
    pub fn new(options: &Options, tx: mpsc::Sender<Frame>) -> Result<Self, LinCapError> {
        let connection = dbus::blocking::Connection::new_session().map_err(|e|
            LinCapError::new(format!("Cannot connect to the capture session bus; launch from the graphical session or configure its bus address: {e}")))?;
        let (portal_stream, restore_token, remote) = ScreenCastPortal::new(&connection)?
            .show_cursor(options.show_cursor)?
            .restore_token(options.restore_token.clone())
            .create_stream()?;
        let stream_id = portal_stream.pw_node_id();
        eprintln!("capture: portal selected PipeWire node {stream_id}");

        // TODO: Fix this hack
        let options = options.clone();
        let (ready_sender, ready_recv) = sync_channel(1);
        let capturer_join_handle = std::thread::spawn(move || {
            let res = pipewire_capturer(options, tx, &ready_sender, stream_id, remote);
            if let Err(error) = &res {
                eprintln!("capture: PipeWire setup failed: {error}");
                let _ = ready_sender.send(false);
            }
            res
        });

        if ready_recv.recv() != Ok(true) {
            return Err(LinCapError::new(
                "Failed to initialize the portal PipeWire remote".into(),
            ));
        }

        Ok(Self {
            capturer_join_handle: Some(capturer_join_handle),
            _connection: connection,
            restore_token,
        })
    }

    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    pub fn start_capture(&self) {
        CAPTURER_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stop_capture(&mut self) {
        CAPTURER_STATE.store(2, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.capturer_join_handle.take() {
            if let Err(e) = handle.join().expect("Failed to join capturer thread") {
                eprintln!("Error occured capturing: {e}");
            }
        }
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn create_capturer(
    options: &Options,
    tx: mpsc::Sender<Frame>,
) -> Result<LinuxCapturer, LinCapError> {
    LinuxCapturer::new(options, tx)
}

impl Drop for LinuxCapturer {
    fn drop(&mut self) {
        self.stop_capture();
    }
}
