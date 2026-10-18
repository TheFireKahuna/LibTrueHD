# LibTrueHD

A C-callable decoder for lossless object-based audio on Apple platforms: Dolby TrueHD streams,
including the Dolby Atmos object presentation: the bed channels, up to 15 dynamic objects, and the
object audio metadata (OAMD) that positions them, with sample-accurate update timing. It ships as
`TrueHD.xcframework` with a Swift package, for iOS, tvOS, macOS and visionOS.

Used by [AetherEngine](https://github.com/superuser404notfound/AetherEngine) to keep TrueHD Atmos
heights on Apple TV and iPhone: Apple devices cannot bitstream TrueHD, so the engine renders the
decoded objects into a speaker bed and delivers it as Apple Positional Audio, which tvOS sends to an
Atmos receiver as Dolby MAT and iOS renders as Spatial Audio.

The decoding is the [`truehd`](https://github.com/truehdd/truehdd) crate (Apache-2.0), pinned to
an exact commit. Until it is merged upstream that commit is in
[a fork](https://github.com/superuser404notfound/truehdd/tree/perf/harletty-block-decode):
upstream plus [harletty's](https://github.com/harletty/truehdd/tree/fix/huffman-tree-build-memory)
speed work on the decoder, which includes [truehdd#42](https://github.com/truehdd/truehdd/pull/42)
(byte-identical output, about half the decode time of the unmodified crate). This repository adds the C API, an access-unit framer that reports where each
decoded block sits in the input (needed to place audio on a timeline after a seek), OAMD update
timing per ETSI TS 103 420 clause 5.3, and the Apple packaging.

Written by [Quick104](https://github.com/Quick104) for [Silo](https://github.com/Silo-Server) as
SiloObjectAudio, transferred here in October 2026 ([AetherEngine#731](https://github.com/superuser404notfound/AetherEngine/issues/731)). Releases before 2.0.0 carry
the old product name.

## Use

```swift
.package(url: "https://github.com/superuser404notfound/LibTrueHD", .upToNextMinor(from: "2.3.1")),
// target dependency:
.product(name: "TrueHD", package: "LibTrueHD"),
```

```swift
import TrueHD

let decoder = truehd_atmos_decoder_create(TRUEHD_ATMOS_PRESENTATION_HIGHEST)!
defer { truehd_atmos_decoder_destroy(decoder) }

// For every demuxed TrueHD packet:
truehd_atmos_decoder_push(decoder, bytes, count, Int64.min)
var block = TrueHDAtmosBlock()
while truehd_atmos_decoder_pull(decoder, &block) == TRUEHD_ATMOS_OK {
    // block.channels: planar float32, one plane per element (bed channel or object)
    // block.speakers: each element's speaker code, TRUEHD_ATMOS_SPEAKER_OBJECT for objects
    // block.metadata: OAMD updates starting in this block (positions, gain, size, zones, ...)
    // block.input_frame_offset: input frames before this block since the last reset
}

// On seek:
truehd_atmos_decoder_reset(decoder)
```

Two per-handle options, both off after `create()` and kept across `reset()`:

- `truehd_atmos_decoder_set_bed_companion(decoder, true)`: with the object presentation
  selected, every block also carries the stream's highest channel-based presentation (the 7.1
  or 5.1 bed) in `block.bed_channels` / `block.bed_speakers`, decoded from the same access
  unit, so the two are sample-aligned without a second decoder.
- `truehd_atmos_decoder_set_ignore_interpolation(decoder, true)`: matrix interpolation values
  are read and discarded, so each matrix holds its last transmitted coefficients until the next
  update. Not bit-exact on streams that interpolate.

The library is built WITHOUT the `matrix-interpolation` Cargo feature by default: the interpolating
decode is then not compiled in at all and every handle behaves as if `set_ignore_interpolation` were
on. Channel-based presentations carry no interpolation and stay bit-exact; an object presentation
that interpolates decodes approximately. `TRUEHD_ATMOS_FEATURES=matrix-interpolation ./build.sh`
builds the exact decoder (and `harness/verify.sh` checks it against an exact truehdd).

[`include/truehd_atmos.h`](include/truehd_atmos.h) is the reference: ownership, threading,
coordinates (OAMD room coordinates, x left→right, y front→back, z floor −1 / ear level 0 /
ceiling +1), the input timeline, presentation selection and every metadata field are documented
there.

## Build

```sh
./build.sh
```

Requires Xcode and rustup. The toolchain and targets are pinned in `rust-toolchain.toml` (stable
Rust; every Apple target used here ships a prebuilt std). The script builds each slice with fat
LTO, prelinks it so only the `truehd_atmos_*` functions and a weak `rust_eh_personality` are global (so it links next to other
Rust static libraries such as LibDovi, and shares their personality routine: compact unwind encodes at most three per image), and assembles the xcframework. Source paths are remapped
so the binaries carry no build-machine paths. Minimum OS: iOS 18, tvOS 18, macOS 15, visionOS 1.

## Verify

```sh
harness/verify.sh
```

Needs Homebrew `ffmpeg`. It downloads Dolby's "Unfold" demo and 7.1.4 Atmos test file from
archive.org, clones `truehdd` at the pinned commit and builds its CLI for reference decodes, then
checks the macOS slice: PCM bit-exact against `truehdd`, object positions against its
`.atmos.metadata`, reset and seek with an exact input timeline, and arbitrary input chunking. It
also links every other slice and imports the module from Swift. Working files go to `.work/`.

`cargo test --release` covers what the harness does not reach (argument handling, damaged input,
presentation switches) and reads the same `.work/` (or `$TRUEHD_ATMOS_WORK`), so run it after
`harness/verify.sh`.

`truehdd`'s metadata export omits the `32 × block_offset_factor` term TS 103 420 5.3 specifies,
so some of its update times are 32 frames earlier than this library's; the verifier accounts for
that difference.

Measured on an M1, one thread, wall time through the C API (`cargo run --release --example
decode_bench <file.thd>`): 45× realtime for Unfold's 16-element Atmos presentation, against 29×
with truehdd#42 alone and 21× before it.

## Limitations

- Intermediate spatial format (ISF) object positions are not decoded; no content with them was found.
- Screen-anchored positions are reported raw with their screen and depth factors.
- A 96 kHz Atmos stream would decode at 96 kHz; MLP has no reduced-rate path.

## License

The code in this repository (C API, framer, build and verification scripts) is MIT, see
[LICENSE](LICENSE). The built library also contains the
`truehd` and `oamd` crates, which are Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), and other Rust
dependencies; [NOTICE](NOTICE) lists them, and anyone redistributing the xcframework must keep
both files with it.

Dolby, Dolby Atmos and Dolby TrueHD are trademarks of Dolby Laboratories. This project is not
affiliated with or endorsed by Dolby Laboratories.
