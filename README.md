# screen-streamer

PetPlay's native capture helper: PipeWire/ScreenCast portal on Linux and Windows Graphics
Capture on Windows. Linux input injection uses `/dev/uinput`. The patched `scap` dependency is
vendored under `vendor/scap`, including its MIT license.

Build with Rust and the platform SDK/development libraries:

```sh
cargo build --locked --release
target/release/screen-streamer --version
```

Linux requires pkg-config, Clang, PipeWire development headers and D-Bus development headers
(`libpipewire-0.3-dev libdbus-1-dev` on Ubuntu). Windows uses the MSVC Rust toolchain and Windows
SDK. Cross builds require the corresponding target toolchain, linker and native library sysroot.

The helper connects to a local TCP receiver. Supported options are `--fps=<1..240>` (default 60),
`--port=<port>` (default 12345), and `--capture-token-path=<file>` for persistent portal restoration.
`--version` exits without starting capture or requesting permissions.

The canonical TypeScript receiver and process supervisor live in PetPlay's
`classes/ScreenCapturer/`; the TS files in this repository are older standalone examples.
PetPlay pins this repository as a submodule, compiles the helper for each release target and
embeds it in its executable. The runtime extracts that helper when capture starts.

Linux capture needs a working graphical session, session D-Bus and a ScreenCast portal.
An executable startup/build check does not establish capture permission or frame delivery.
