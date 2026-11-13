//! Publish captured frames to the native media worker's DMA-BUF producer contract.
//!
//! The capture backend still delivers CPU pixels, so each frame is copied once into a
//! linear GBM buffer. Such a buffer is both CPU-writable and DMA-BUF exportable, which
//! lets the GPU encoder import it through its strict DMA-BUF path without a readback of
//! its own. Giving the capture backend a GPU buffer later changes only this file: the
//! wire contract, the encoder and the receiver never learn where the pixels came from.
//!
//! The contract is `petplay_mediatransport/docs/protocol.md`: one 64-byte SOCK_SEQPACKET
//! message per frame carrying the DMA-BUF through SCM_RIGHTS, and a 16-byte acknowledgement
//! that returns the slot. A full pool drops the capture attempt rather than delaying it.

use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};
use std::fs::{File, OpenOptions};
use std::io::{self, Error, ErrorKind};
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

const MAGIC: u32 = 0x5050_4d31;
const VERSION: u32 = 1;
const SLOTS: usize = 3;
const HEADER: usize = 64;
const ACK: usize = 16;
/// The producer writes with the CPU and the write is complete before the descriptor is
/// sent, so there is no fence to pass and the frame is already signaled.
const FLAG_ALREADY_SIGNALED: u32 = 1;
/// `VK_FORMAT_B8G8R8A8_UNORM`, matching DRM `ARGB8888` and the capture backend's BGRA.
const FORMAT_B8G8R8A8_UNORM: u32 = 44;
/// The worker rejects a larger pitch or offset, so reject it here with a clearer message.
const LAYOUT_LIMIT: u32 = 65536;

fn unsupported(message: impl Into<String>) -> Error {
  Error::new(ErrorKind::Unsupported, message.into())
}

fn monotonic_nanoseconds() -> u64 {
  let mut now = libc::timespec {
    tv_sec: 0,
    tv_nsec: 0,
  };
  // CLOCK_MONOTONIC cannot fail for a valid timespec, and the worker only needs
  // differences between successive capture timestamps.
  unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
  now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

struct Slot {
  buffer: BufferObject<()>,
  pitch: u32,
  /// The sequence the worker still owes an acknowledgement for.
  pending: Option<u64>,
}

pub struct MediaProducer {
  socket: OwnedFd,
  /// Declared before the device so every buffer is released while it is still open.
  slots: Vec<Slot>,
  _device: Device<File>,
  width: u32,
  height: u32,
  sequence: u64,
  published: u64,
  dropped: u64,
}

impl MediaProducer {
  /// Allocate the slot pool and connect to a worker that is already listening.
  pub fn connect(
    socket_path: &Path,
    render_node: &Path,
    width: u32,
    height: u32,
  ) -> io::Result<Self> {
    if width % 2 != 0 || height % 2 != 0 || !(16..=8192).contains(&width) || !(16..=8192).contains(&height) {
      return Err(unsupported(format!(
        "{width}x{height} is not an even size the H.264 encoder accepts"
      )));
    }
    let node = OpenOptions::new()
      .read(true)
      .write(true)
      .open(render_node)
      .map_err(|error| Error::new(error.kind(), format!("open {}: {error}", render_node.display())))?;
    let device = Device::new(node)
      .map_err(|error| Error::new(error.kind(), format!("create a GBM device: {error}")))?;
    let mut slots = Vec::with_capacity(SLOTS);
    for _ in 0..SLOTS {
      // Ask for the linear modifier explicitly. The encoder imports with modifier zero,
      // so a driver that answers with a tiled layout has to fail here, not there.
      let buffer = device.create_buffer_object_with_modifiers2::<()>(
        width,
        height,
        Format::Argb8888,
        [Modifier::Linear].into_iter(),
        // The explicit linear modifier already fixes the layout. Mesa rejects
        // `USE_LINEAR` alongside a modifier list with EINVAL.
        BufferObjectFlags::RENDERING,
      )
      .map_err(|error| {
        Error::new(
          error.kind(),
          format!("allocate a linear {width}x{height} ARGB8888 buffer: {error}"),
        )
      })?;
      let pitch = buffer.stride();
      if buffer.modifier() != Modifier::Linear
        || buffer.plane_count() != 1
        || buffer.offset(0) != 0
        || pitch < width * 4
        || pitch > LAYOUT_LIMIT
      {
        return Err(unsupported(format!(
          "unusable buffer layout: modifier {:?} planes {} offset {} pitch {pitch}",
          buffer.modifier(),
          buffer.plane_count(),
          buffer.offset(0),
        )));
      }
      slots.push(Slot {
        buffer,
        pitch,
        pending: None,
      });
    }
    let socket = connect_seqpacket(socket_path).map_err(|error| {
      Error::new(
        error.kind(),
        format!("connect to the media worker at {}: {error}", socket_path.display()),
      )
    })?;
    Ok(Self {
      socket,
      slots,
      _device: device,
      width,
      height,
      sequence: 0,
      published: 0,
      dropped: 0,
    })
  }

  pub fn published(&self) -> u64 {
    self.published
  }

  pub fn dropped(&self) -> u64 {
    self.dropped
  }

  /// The encoded size, which crops an odd capture width or height by one pixel.
  pub fn geometry(&self) -> (u32, u32) {
    (self.width, self.height)
  }

  /// Copy one captured frame into a free slot and hand its DMA-BUF to the worker.
  /// Returns `false` when every slot is still being encoded, which drops the frame.
  ///
  /// `source_width` and `source_height` describe the captured image. Its top-left
  /// encoded-size region is published, so an odd capture size loses its last row or
  /// column rather than failing. A capture that shrinks ends the session: the encoder
  /// holds one fixed geometry for the stream a receiver has already been told about.
  pub fn publish(
    &mut self,
    bgra: &[u8],
    source_width: u32,
    source_height: u32,
  ) -> io::Result<bool> {
    let (width, height) = (self.width, self.height);
    if source_width < width || source_height < height {
      return Err(unsupported(format!(
        "capture resized to {source_width}x{source_height}; the encoder session is fixed at {width}x{height}"
      )));
    }
    let row = width as usize * 4;
    let source_row = source_width as usize * 4;
    if bgra.len() < source_row * source_height as usize {
      return Err(unsupported("captured frame is shorter than its geometry"));
    }
    self.collect_acknowledgements()?;
    let Some(index) = self.slots.iter().position(|slot| slot.pending.is_none()) else {
      self.dropped += 1;
      return Ok(false);
    };
    let sequence = self.sequence + 1;
    let slot = &mut self.slots[index];
    let pitch = slot.pitch as usize;
    slot.buffer.map_mut(0, 0, width, height, |mapped| {
      let stride = mapped.stride() as usize;
      if stride != pitch {
        return Err(unsupported("buffer stride changed between frames"));
      }
      let destination = mapped.buffer_mut();
      if destination.len() < stride * height as usize {
        return Err(unsupported("mapped buffer is smaller than the frame"));
      }
      for y in 0..height as usize {
        destination[y * stride..y * stride + row]
          .copy_from_slice(&bgra[y * source_row..y * source_row + row]);
      }
      Ok(())
    })??;
    let mut header = [0u8; HEADER];
    header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&(index as u32).to_le_bytes());
    header[12..16].copy_from_slice(&FLAG_ALREADY_SIGNALED.to_le_bytes());
    header[16..24].copy_from_slice(&sequence.to_le_bytes());
    header[24..32].copy_from_slice(&monotonic_nanoseconds().to_le_bytes());
    header[32..36].copy_from_slice(&width.to_le_bytes());
    header[36..40].copy_from_slice(&height.to_le_bytes());
    header[40..44].copy_from_slice(&slot.pitch.to_le_bytes());
    // Offset zero and modifier zero were both verified when the slot was allocated.
    header[56..60].copy_from_slice(&FORMAT_B8G8R8A8_UNORM.to_le_bytes());
    let exported = slot.buffer.fd().map_err(|error| {
      Error::new(ErrorKind::Other, format!("export the slot's DMA-BUF: {error}"))
    })?;
    send_descriptor(self.socket.as_fd(), &header, exported.as_fd())?;
    self.slots[index].pending = Some(sequence);
    self.sequence = sequence;
    self.published += 1;
    Ok(true)
  }

  /// Return every slot the worker has finished with. Never blocks on the worker.
  fn collect_acknowledgements(&mut self) -> io::Result<()> {
    loop {
      let mut bytes = [0u8; ACK];
      let read = unsafe {
        libc::recv(
          self.socket.as_raw_fd(),
          bytes.as_mut_ptr().cast(),
          bytes.len(),
          libc::MSG_DONTWAIT,
        )
      };
      if read < 0 {
        let error = Error::last_os_error();
        return match error.kind() {
          ErrorKind::WouldBlock | ErrorKind::Interrupted => Ok(()),
          _ => Err(error),
        };
      }
      if read == 0 {
        return Err(Error::new(
          ErrorKind::BrokenPipe,
          "the media worker closed the producer socket",
        ));
      }
      if read as usize != ACK || u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != MAGIC {
        return Err(unsupported("malformed slot release"));
      }
      let slot = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
      let sequence = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
      match self.slots.get_mut(slot) {
        Some(entry) if entry.pending == Some(sequence) => entry.pending = None,
        _ => return Err(unsupported("slot release does not match an outstanding frame")),
      }
    }
  }
}

fn connect_seqpacket(path: &Path) -> io::Result<OwnedFd> {
  use std::os::fd::FromRawFd;
  use std::os::unix::ffi::OsStrExt;
  let bytes = path.as_os_str().as_bytes();
  let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
    return Err(unsupported("invalid media socket path"));
  }
  address.sun_family = libc::AF_UNIX as _;
  for (index, byte) in bytes.iter().enumerate() {
    address.sun_path[index] = *byte as _;
  }
  let raw = unsafe {
    libc::socket(
      libc::AF_UNIX,
      libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
      0,
    )
  };
  if raw < 0 {
    return Err(Error::last_os_error());
  }
  let socket = unsafe { OwnedFd::from_raw_fd(raw) };
  let connected = unsafe {
    libc::connect(
      socket.as_raw_fd(),
      &address as *const _ as *const _,
      size_of::<libc::sockaddr_un>() as _,
    )
  };
  if connected < 0 {
    return Err(Error::last_os_error());
  }
  Ok(socket)
}

fn send_descriptor(socket: BorrowedFd, header: &[u8; HEADER], image: BorrowedFd) -> io::Result<()> {
  let mut io_vector = libc::iovec {
    iov_base: header.as_ptr() as *mut _,
    iov_len: header.len(),
  };
  let mut control = [0usize; 8];
  let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
  message.msg_iov = &raw mut io_vector;
  message.msg_iovlen = 1;
  message.msg_control = control.as_mut_ptr().cast();
  message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
  unsafe {
    let header_pointer = libc::CMSG_FIRSTHDR(&message);
    (*header_pointer).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as u32) as usize;
    (*header_pointer).cmsg_level = libc::SOL_SOCKET;
    (*header_pointer).cmsg_type = libc::SCM_RIGHTS;
    *libc::CMSG_DATA(header_pointer).cast::<i32>() = image.as_raw_fd();
  }
  let sent = unsafe {
    libc::sendmsg(
      socket.as_raw_fd(),
      &raw const message,
      libc::MSG_NOSIGNAL,
    )
  };
  if sent < 0 {
    return Err(Error::last_os_error());
  }
  if sent as usize != header.len() {
    return Err(unsupported("short frame descriptor"));
  }
  Ok(())
}
