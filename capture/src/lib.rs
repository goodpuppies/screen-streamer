//! GPU screen capture for encoders in the same process.
//!
//! The screen-streamer binary hands frames to other processes as CPU pixels (the overlay's frame
//! socket) or copies them into DMA-BUFs (Linux's media socket). An encoder that links this crate
//! instead receives each frame as a texture on its own GPU device and reads it there: no pixel
//! leaves the GPU.
//!
//! Windows: [`windows::Capture`], Windows Graphics Capture of one monitor per session, on the
//! caller's D3D11 device.
#[cfg(windows)]
pub mod windows;
