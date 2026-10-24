#!/usr/bin/env bash
# verify.sh - verify TrueHD.xcframework against truehdd's own decoder.
#
# Usage: harness/verify.sh        (after ./build.sh)
#
# Working files (samples, the truehdd checkout, reference decodes) live in $TRUEHD_ATMOS_WORK,
# default .work/ in this repository (gitignored).
#
# 1. Fetches the test samples into $TRUEHD_ATMOS_WORK/samples if missing (Dolby "Unfold" demo and the first
#    120 s of Dolby's 7.1.4 Atmos test file, both from archive.org; FFmpeg's FATE atmos.thd).
# 2. Clones truehdd at the commit Cargo.toml pins, builds its CLI and writes reference
#    decodes into ../ref.
# 3. Builds the C harness against the xcframework's macOS slice and runs it: bit-exact PCM,
#    object metadata against truehdd's .atmos.metadata, reset/seek, arbitrary chunking.
# 4. Links a small C program against every other slice, and compiles and runs a Swift program
#    through the module map (`import TrueHD`).
# Requires Homebrew ffmpeg (libavformat for demuxing, the ffmpeg CLI for sample extraction).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(dirname "$HERE")"
ROOT="${TRUEHD_ATMOS_WORK:-$CRATE/.work}"
SAMPLES="$ROOT/samples"
# A library built without `matrix-interpolation` (TRUEHD_ATMOS_NO_INTERPOLATION=1) is checked
# against truehdd built the same way, so the references and the reference CLI are kept apart per build.
if [ -n "${TRUEHD_ATMOS_NO_INTERPOLATION:-}" ]; then
    REF="$ROOT/ref-nointerp"; CLI_FLAGS=(--no-default-features); CLI_TARGET=target-nointerp
else
    REF="$ROOT/ref"; CLI_FLAGS=(); CLI_TARGET=target
fi
XCFW="$CRATE/TrueHD.xcframework"
OUT="$CRATE/build/verify"
ASSETS="$ROOT/truehdd/truehd/tests/assets"
for dir in /opt/homebrew/opt/rustup/bin "$HOME/.cargo/bin"; do
    [ -d "$dir" ] && PATH="$dir:$PATH"
done
export PATH

[ -d "$XCFW" ] || { echo "ERROR: build the xcframework first (./build.sh)" >&2; exit 1; }
mkdir -p "$SAMPLES" "$REF/fixtures" "$OUT"

# ---- samples ----------------------------------------------------------------------------
UNFOLD_URL='https://archive.org/download/dolby-unfold-lossless-7.1/Dolby%20Unfold%20%5BLossless%5D%20%5B7.1%5D.m2ts'
DOLBY714_URL='https://archive.org/download/dolby-atmos-true-hd-e-ac-3-7.1.4-7.1.4-dolby-atmos/Dolby%20Atmos%20TrueHD%2C%20E-AC-3%207.1.4%5B7.1.4%20dolby%20atmos%5D.mkv'
if [ ! -f "$SAMPLES/dolby-unfold-lossless.m2ts" ]; then
    echo "==> Fetching Dolby Unfold demo (91 MB)"
    curl -fsSL -o "$SAMPLES/dolby-unfold-lossless.m2ts" "$UNFOLD_URL"
fi
[ -f "$SAMPLES/dolby-unfold.thd" ] ||
    ffmpeg -nostdin -loglevel error -i "$SAMPLES/dolby-unfold-lossless.m2ts" -map 0:a:0 -c copy -f truehd "$SAMPLES/dolby-unfold.thd"
if [ ! -f "$SAMPLES/dolby-714-test-120s.mka" ]; then
    echo "==> Fetching the first 120 s of Dolby's 7.1.4 Atmos test file (audio only)"
    ffmpeg -nostdin -loglevel error -rw_timeout 60000000 -i "$DOLBY714_URL" -map 0:a:0 -c copy -t 120 "$SAMPLES/dolby-714-test-120s.mka"
fi
[ -f "$SAMPLES/dolby-714-test-120s.thd" ] ||
    ffmpeg -nostdin -loglevel error -i "$SAMPLES/dolby-714-test-120s.mka" -map 0:a:0 -c copy -f truehd "$SAMPLES/dolby-714-test-120s.thd"
[ -f "$SAMPLES/fate-atmos.thd" ] ||
    curl -fsSL -o "$SAMPLES/fate-atmos.thd" https://fate-suite.ffmpeg.org/truehd/atmos.thd

# ---- references from truehdd -------------------------------------------------------------
TRUEHDD_URL="$(sed -nE 's/^truehd = .*git = "([^"]+)".*/\1/p' "$CRATE/Cargo.toml")"
TRUEHDD_REV="$(sed -nE 's/^truehd = .*rev = "([0-9a-f]+)".*/\1/p' "$CRATE/Cargo.toml")"
if [ ! -d "$ROOT/truehdd/.git" ]; then
    echo "==> Cloning truehdd at $TRUEHDD_REV"
    git clone --quiet "$TRUEHDD_URL" "$ROOT/truehdd"
fi
# A checkout made from another remote (upstream before the pin moved to a fork) lacks the commit.
git -C "$ROOT/truehdd" cat-file -e "$TRUEHDD_REV^{commit}" 2>/dev/null ||
    git -C "$ROOT/truehdd" fetch --quiet "$TRUEHDD_URL" "$TRUEHDD_REV"
git -C "$ROOT/truehdd" checkout --quiet "$TRUEHDD_REV"
TRUEHDD="$ROOT/truehdd/$CLI_TARGET/release/truehdd"
# Always through cargo, which rebuilds only what changed: a binary left from another commit or
# feature set would otherwise decode the references.
echo "==> Building the truehdd CLI ${CLI_FLAGS[*]:-}"
(cd "$ROOT/truehdd" && cargo build --release --locked --target-dir "$CLI_TARGET" ${CLI_FLAGS[@]+"${CLI_FLAGS[@]}"})
# References are reused across runs, so they are tied to the CLI that made them.
STAMP="$TRUEHDD_REV ${CLI_FLAGS[*]:-}"
if [ "$(cat "$REF/.made-by" 2>/dev/null)" != "$STAMP" ]; then
    rm -rf "$REF"
    mkdir -p "$REF/fixtures"
    printf '%s\n' "$STAMP" >"$REF/.made-by"
fi
reference() { # <output base> <truehdd decode args...>
    local base="$1"
    shift
    [ -e "$base.atmos.audio" ] || [ -e "$base.caf" ] ||
        "$TRUEHDD" --loglevel error decode --format caf --output-path "$base" "$@"
}
reference "$REF/unfold_p3" --presentation 3 "$SAMPLES/dolby-unfold.thd"
reference "$REF/unfold_p2" --presentation 2 "$SAMPLES/dolby-unfold.thd"
reference "$REF/dolby120" --presentation 3 "$SAMPLES/dolby-714-test-120s.thd"
for f in "$ASSETS"/*.mlp; do
    reference "$REF/fixtures/$(basename "$f" .mlp)" --presentation max "$f"
done

# ---- harness against the macOS slice ------------------------------------------------------
MAC="$XCFW/macos-arm64_x86_64"
clang -std=c11 -O2 -Wall -Wextra -Wno-unused-parameter -mmacosx-version-min=15.0 \
    -I"$MAC/Headers/TrueHD" $(pkg-config --cflags libavformat) \
    -o "$OUT/harness" "$HERE/harness.c" "$MAC/libtruehd_atmos.a" \
    $(pkg-config --libs libavformat libavcodec libavutil)
H="$OUT/harness"

failures=0
run() { # <label> <harness args...>
    local label="$1" status=0
    shift
    local log="$OUT/$(printf '%s' "$label" | tr -c 'A-Za-z0-9' '_').log"
    echo
    echo "======== $label"
    "$H" "$@" >"$log" 2>&1 || status=$?
    grep -v 'no PTS found at end of file' "$log" || true
    if [ "$status" -ne 0 ]; then
        echo "!!!!!!!! FAILED: $label"
        failures=$((failures + 1))
    fi
}
# The first case prints the per-second motion summary; the rest only their checks.
run "Unfold demo (M2TS, 1 AU per packet), p3 + OAMD + seek" \
    --objects 0,1,2,3 --ref-audio "$REF/unfold_p3.atmos.audio" \
    --ref-metadata "$REF/unfold_p3.atmos.metadata" --reset-at 8 --resume-at 15 \
    "$SAMPLES/dolby-unfold-lossless.m2ts"
run "Unfold demo (raw .thd, random 1..16384-byte chunks), seek into mid-access-unit" \
    --no-summary --ref-audio "$REF/unfold_p3.atmos.audio" \
    --ref-metadata "$REF/unfold_p3.atmos.metadata" --reset-at 5 --resume-at 20 \
    "$SAMPLES/dolby-unfold.thd"
run "Dolby 7.1.4 test (MKA, 120 s), p3 + OAMD + seek" \
    --no-summary --ref-audio "$REF/dolby120.atmos.audio" \
    --ref-metadata "$REF/dolby120.atmos.metadata" --reset-at 30 --resume-at 90 \
    "$SAMPLES/dolby-714-test-120s.mka"
run "Unfold demo, 7.1 fallback (presentation 2) + seek" \
    --no-summary --presentation 2 --ref-audio "$REF/unfold_p2.caf" --reset-at 3 --resume-at 11 \
    "$SAMPLES/dolby-unfold-lossless.m2ts"
run "Unfold demo, HIGHEST_CHANNEL_BASED selector" \
    --no-summary --presentation channel --ref-audio "$REF/unfold_p2.caf" "$SAMPLES/dolby-unfold.thd"
run "FFmpeg FATE atmos.thd" --no-summary "$SAMPLES/fate-atmos.thd"
for f in "$ASSETS"/*.mlp; do
    b="$(basename "$f" .mlp)"
    if [ -f "$REF/fixtures/$b.atmos.audio" ]; then
        run "fixture $b" --no-summary --ref-audio "$REF/fixtures/$b.atmos.audio" \
            --ref-metadata "$REF/fixtures/$b.atmos.metadata" "$f"
    else
        run "fixture $b" --no-summary --ref-audio "$REF/fixtures/$b.caf" "$f"
    fi
done

# ---- every slice links; Swift sees the module ----------------------------------------------
echo
echo "======== link check per slice"
cat >"$OUT/link.c" <<'EOF'
#include "truehd_atmos.h"
int main(void) {
    TrueHDAtmosDecoder *d = truehd_atmos_decoder_create(TRUEHD_ATMOS_PRESENTATION_HIGHEST);
    TrueHDAtmosBlock b;
    int st = truehd_atmos_decoder_pull(d, &b);
    truehd_atmos_decoder_destroy(d);
    return truehd_atmos_selftest() == 0 && st == TRUEHD_ATMOS_NEED_MORE_DATA ? 0 : 1;
}
EOF
for spec in "ios-arm64 arm64-apple-ios18.0 iphoneos" \
    "ios-arm64_x86_64-simulator arm64-apple-ios18.0-simulator iphonesimulator" \
    "ios-arm64_x86_64-simulator x86_64-apple-ios18.0-simulator iphonesimulator" \
    "tvos-arm64 arm64-apple-tvos18.0 appletvos" \
    "tvos-arm64-simulator arm64-apple-tvos18.0-simulator appletvsimulator" \
    "xros-arm64 arm64-apple-xros2.0 xros" \
    "xros-arm64-simulator arm64-apple-xros2.0-simulator xrsimulator" \
    "macos-arm64_x86_64 x86_64-apple-macos15.0 macosx"; do
    read -r slice triple sdk <<<"$spec"
    if xcrun --sdk "$sdk" clang -target "$triple" -I"$XCFW/$slice/Headers/TrueHD" "$OUT/link.c" \
        "$XCFW/$slice/libtruehd_atmos.a" -o "$OUT/link-$slice" 2>"$OUT/link-$slice.log"; then
        echo "  $slice ($triple): links"
    else
        echo "  $slice ($triple): LINK FAILED"
        cat "$OUT/link-$slice.log"
        failures=$((failures + 1))
    fi
done
arch -x86_64 "$OUT/link-macos-arm64_x86_64" 2>/dev/null && echo "  macOS x86_64 slice runs under Rosetta: selftest OK" ||
    echo "  (macOS x86_64 slice not run: Rosetta unavailable or failed)"

# ---- next to a second Rust library, in a host with Objective-C and C++ ---------------------
# LibDTSX used to share these steps; since its 2.0.0 it ships a dynamic framework and needs none of them.
# Compact unwind encodes at most three personality routines per image. A second Rust static
# library brings its own rust_eh_personality and the host brings __objc_personality_v0 and
# __gxx_personality_v0, so this links only if ours coalesces with the other Rust copy
# (AetherEngine#752). Both link orders, and the panic selftest must still pass in the result.
echo
echo "======== next to a second Rust library, Objective-C and C++"
cat >"$OUT/other_rust.rs" <<'EOF'
#[unsafe(no_mangle)]
pub extern "C" fn other_rust_catches() -> i32 {
    std::panic::catch_unwind(|| panic!("other_rust: deliberate panic, caught")).is_err() as i32
}
EOF
cat >"$OUT/host_objc.m" <<'EOF'
#import <Foundation/Foundation.h>
#include "truehd_atmos.h"
int host_cxx_catches(void);
int other_rust_catches(void);
int main(void) {
    int objc = 0;
    @try { @throw [NSException exceptionWithName:@"host" reason:@"caught" userInfo:nil]; }
    @catch (NSException *e) { objc = 1; }
    return objc && host_cxx_catches() && other_rust_catches() && truehd_atmos_selftest() == 0 ? 0 : 1;
}
EOF
cat >"$OUT/host_cxx.cpp" <<'EOF'
#include <stdexcept>
extern "C" int host_cxx_catches(void) {
    try { throw std::runtime_error("host"); } catch (const std::exception &) { return 1; }
}
EOF
if (cd "$CRATE" && rustc --edition 2024 -O --crate-type staticlib --target aarch64-apple-darwin \
        "$OUT/other_rust.rs" -o "$OUT/libother_rust.a") 2>"$OUT/coexist.log" &&
    xcrun clang -c -mmacosx-version-min=15.0 -fobjc-exceptions -I"$MAC/Headers/TrueHD" \
        "$OUT/host_objc.m" -o "$OUT/host_objc.o" 2>>"$OUT/coexist.log" &&
    xcrun clang++ -c -mmacosx-version-min=15.0 "$OUT/host_cxx.cpp" -o "$OUT/host_cxx.o" 2>>"$OUT/coexist.log"; then
    for order in other-first truehd-first; do
        if [ "$order" = other-first ]; then libs=("$OUT/libother_rust.a" "$MAC/libtruehd_atmos.a")
        else libs=("$MAC/libtruehd_atmos.a" "$OUT/libother_rust.a"); fi
        if xcrun clang++ -mmacosx-version-min=15.0 "$OUT/host_objc.o" "$OUT/host_cxx.o" "${libs[@]}" \
                -framework Foundation -o "$OUT/coexist-$order" 2>>"$OUT/coexist.log" &&
            "$OUT/coexist-$order" 2>/dev/null; then
            echo "  $order: links with one Rust personality, all four catch"
        else
            echo "  $order: FAILED"
            cat "$OUT/coexist.log"
            failures=$((failures + 1))
        fi
    done
else
    echo "  COEXISTENCE CHECK COULD NOT BUILD ITS HOST"
    cat "$OUT/coexist.log"
    failures=$((failures + 1))
fi

echo
echo "======== Swift through the module map"
cat >"$OUT/swift_check.swift" <<'EOF'
import Foundation
import TrueHD

let data = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
guard let decoder = truehd_atmos_decoder_create(TRUEHD_ATMOS_PRESENTATION_HIGHEST) else { exit(1) }
defer { truehd_atmos_decoder_destroy(decoder) }
_ = data.withUnsafeBytes { truehd_atmos_decoder_push(decoder, $0.bindMemory(to: UInt8.self).baseAddress, $0.count, 0) }
var block = TrueHDAtmosBlock()
var blocks = 0, updates = 0
while truehd_atmos_decoder_pull(decoder, &block) == TRUEHD_ATMOS_OK {
    blocks += 1
    if let md = block.metadata { updates += Int(md.pointee.update_count) }
}
var info = TrueHDAtmosStreamInfo()
_ = truehd_atmos_decoder_get_stream_info(decoder, &info)
print("Swift: \(blocks) blocks, p\(info.selected_presentation) \(block.channel_count) ch @ \(info.sample_rate) Hz, \(updates) metadata updates, selftest \(truehd_atmos_selftest())")
exit(blocks > 0 ? 0 : 1)
EOF
if xcrun swiftc -O -I "$MAC/Headers/TrueHD" "$OUT/swift_check.swift" "$MAC/libtruehd_atmos.a" \
    -o "$OUT/swift_check" 2>"$OUT/swift.log" && "$OUT/swift_check" "$ASSETS/fba_atmos_obj.mlp" 2>/dev/null; then
    :
else
    echo "  SWIFT CHECK FAILED"
    cat "$OUT/swift.log"
    failures=$((failures + 1))
fi

# ---- optional: run the tvOS simulator slice in a temporary simulator ------------------------
# VERIFY_TVOS_SIM=1 creates an Apple TV simulator, decodes the Unfold demo there with the
# tvos-arm64-simulator slice, and compares the digest with the macOS slice's. The simulator is
# deleted afterwards; no existing simulator is touched.
if [ "${VERIFY_TVOS_SIM:-0}" = 1 ]; then
    echo
    echo "======== tvOS simulator (tvos-arm64-simulator slice) vs macOS slice"
    xcrun clang -O2 -mmacosx-version-min=15.0 -I"$MAC/Headers/TrueHD" "$HERE/checksum.c" \
        "$MAC/libtruehd_atmos.a" -o "$OUT/checksum-macos"
    xcrun --sdk appletvsimulator clang -O2 -target arm64-apple-tvos18.0-simulator \
        -I"$XCFW/tvos-arm64-simulator/Headers/TrueHD" "$HERE/checksum.c" \
        "$XCFW/tvos-arm64-simulator/libtruehd_atmos.a" -o "$OUT/checksum-tvos-sim"
    runtime="$(xcrun simctl list runtimes | awk '/^tvOS/ {id=$NF} END {print id}')"
    devtype="$(xcrun simctl list devicetypes | grep -o 'com.apple.CoreSimulator.SimDeviceType.Apple-TV-4K[^)]*' | head -1)"
    udid="$(xcrun simctl create "LibTrueHD verify" "$devtype" "$runtime")"
    trap 'xcrun simctl shutdown "$udid" >/dev/null 2>&1; xcrun simctl delete "$udid" >/dev/null 2>&1' EXIT
    xcrun simctl boot "$udid"
    host="$("$OUT/checksum-macos" "$SAMPLES/dolby-unfold.thd" 2>/dev/null)"
    sim="$(xcrun simctl spawn "$udid" "$OUT/checksum-tvos-sim" "$SAMPLES/dolby-unfold.thd" 2>/dev/null)"
    echo "  macOS:        $host"
    echo "  tvOS sim:     $sim"
    if [ -n "$sim" ] && [ "$host" = "$sim" ]; then
        echo "  identical output"
    else
        echo "  TVOS SIMULATOR OUTPUT DIFFERS"
        failures=$((failures + 1))
    fi
fi

echo
if [ "$failures" -eq 0 ]; then echo "ALL CHECKS PASSED"; else echo "$failures CHECK(S) FAILED"; exit 1; fi
