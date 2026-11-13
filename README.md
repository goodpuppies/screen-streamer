# screen-streamer

PetPlay's native capture helper: PipeWire/ScreenCast portal on Linux and Windows Graphics
Capture on Windows. Linux input injection uses `/dev/uinput`. The patched `scap` dependency is
vendored under `vendor/scap`, including its MIT license.

Build with Rust and the platform SDK/development libraries:

```sh
cargo build --locked --release
target/release/screen-streamer --version
```

Linux requires pkg-config, Clang, PipeWire development headers, D-Bus development headers and
GBM development headers (`libpipewire-0.3-dev libdbus-1-dev libgbm-dev` on Ubuntu). Windows uses the MSVC Rust toolchain and Windows
SDK. Cross builds require the corresponding target toolchain, linker and native library sysroot.

The helper connects to a local TCP receiver. Supported options are `--fps=<1..240>` (default 60),
`--port=<port>` (default 12345), and `--capture-token-path=<file>` for persistent portal restoration.
`--version` exits without starting capture or requesting permissions. On Linux, `--no-input`
disables uinput setup for capture-only use and isolated tests.

## Publishing to the native encoder (Linux)

`--media-socket=<path>` additionally hands every captured frame to a `petplay-media` worker
already listening on that AF_UNIX path, which encodes it on the GPU for streaming.
`--media-render-node=<path>` selects the DRM render node (default `/dev/dri/renderD128`), and
must match the worker's. `--no-frame-socket` skips the TCP receiver entirely, for streaming
without a local overlay consumer.

The capture backend still delivers CPU pixels, so each frame is copied once into a linear GBM
buffer — CPU-writable and DMA-BUF exportable — and handed over through the worker's strict
DMA-BUF producer contract in `petplay_mediatransport/docs/protocol.md`. The encoder itself never
reads back. An odd capture width or height is cropped by one pixel, because H.264 cannot encode
it; a capture that shrinks mid-session ends the producer, since a receiver has already been told
the stream's geometry. A full slot pool drops the capture attempt rather than delaying capture.

`examples/media_pattern.rs` publishes a moving synthetic pattern through the same code, which
exercises allocation, export, the descriptor, acknowledgements and the GPU encoder without a
capture portal and without putting real screen content into an encoded stream:

```sh
petplay-media /tmp/media.sock - --layout mono --fps 60 &
cargo run --release --example media_pattern -- /tmp/media.sock 1920 1080 120
```

The canonical TypeScript receiver and process supervisor live in PetPlay's
`classes/ScreenCapturer/`; the TS files in this repository are older standalone examples.
PetPlay pins this repository as a submodule, compiles the helper for each release target and
embeds it in its executable. The runtime extracts that helper when capture starts.

Linux capture needs a working graphical session, session D-Bus and a ScreenCast portal.
An executable startup/build check does not establish capture permission or frame delivery.
