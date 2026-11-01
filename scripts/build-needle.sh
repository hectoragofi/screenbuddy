#!/usr/bin/env bash
# Builds .needle/needle.dll: Cactus Compute's Needle engine (runs the Whistle
# speech-to-text model), linked from Cactus's prebuilt windows-x86_64
# libneedle.a with llvm-mingw, libc++ linked in statically. build.rs embeds it.
#
# Set LLVM_MINGW=<dir> to use an existing llvm-mingw instead of downloading one.
set -euo pipefail
cd "$(dirname "$0")/.."

NEEDLE_REV=d6c7d6a2e9655505e0f8e23339356dd1705900eb
NEEDLE_SHA=22d95a38f34c225a6af115c40069ea61360766479222862f1fea8b615ea7718a
MINGW_VER=20261020
MINGW_NAME=llvm-mingw-$MINGW_VER-ucrt-x86_64
MINGW_SHA=fd5462b7f5d93b2ff981fcd3f9c7e427e47f3203c64ea91c27bcc444eebd74cd

OUT=.needle
mkdir -p "$OUT/dl"

fetch() { # url dest sha256
  if [ -f "$2" ] && echo "$3  $2" | sha256sum -c --status; then return; fi
  curl -fL --retry 3 -o "$2.part" "$1"
  if ! echo "$3  $2.part" | sha256sum -c --status; then
    echo "checksum mismatch for $1" >&2
    rm -f "$2.part"
    exit 1
  fi
  mv "$2.part" "$2"
}

fetch "https://huggingface.co/Cactus-Compute/needle3/resolve/$NEEDLE_REV/windows-x86_64/libneedle.a" "$OUT/dl/libneedle.a" "$NEEDLE_SHA"
curl -fsL "https://huggingface.co/Cactus-Compute/needle3/resolve/$NEEDLE_REV/LICENSE" -o "$OUT/LICENSE-needle" || true

if [ -z "${LLVM_MINGW:-}" ]; then
  if [ ! -x "$OUT/dl/$MINGW_NAME/bin/x86_64-w64-mingw32-clang++" ] && [ ! -x "$OUT/dl/$MINGW_NAME/bin/x86_64-w64-mingw32-clang++.exe" ]; then
    fetch "https://github.com/mstorsjo/llvm-mingw/releases/download/$MINGW_VER/$MINGW_NAME.zip" "$OUT/dl/llvm-mingw.zip" "$MINGW_SHA"
    unzip -q -o "$OUT/dl/llvm-mingw.zip" -d "$OUT/dl"
    rm -f "$OUT/dl/llvm-mingw.zip"
  fi
  LLVM_MINGW="$OUT/dl/$MINGW_NAME"
fi

cat > "$OUT/needle.def" <<'DEF'
LIBRARY needle.dll
EXPORTS
  needle_load
  needle_models
  needle_last_error
  needle_transcribe
  needle_stream_transcribe_process
  needle_stream_transcribe_stop
  needle_reset
DEF

"$LLVM_MINGW/bin/x86_64-w64-mingw32-clang++" -shared -s -o "$OUT/needle.dll" \
  -Wl,--whole-archive "$OUT/dl/libneedle.a" -Wl,--no-whole-archive "$OUT/needle.def" \
  -static -lws2_32 -lpsapi
echo "built $OUT/needle.dll ($(wc -c < "$OUT/needle.dll") bytes)"
