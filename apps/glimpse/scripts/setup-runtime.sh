#!/usr/bin/env bash
# Build Glimpse's own inference wrapper against pinned, unmodified llama.cpp.
set -euo pipefail
here=$(cd "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
base="${XDG_DATA_HOME:-$HOME/.local/share}/nixe-glimpse"
build="${XDG_CACHE_HOME:-$HOME/.cache}/nixe-glimpse/inference-build"
jobs="${GLIMPSE_BUILD_JOBS:-4}"
[[ $jobs =~ ^[1-9][0-9]*$ ]] && ((jobs <= 16)) || { echo "Choose 1–16 build jobs." >&2; exit 1; }
for tool in cargo cmake git c++; do command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }; done
umask 077
mkdir -p "$build"
revision=4d756bc72bf00a4aacf410ae15a2d315f3db400d
source="$build/llama.cpp"
if [[ ! -d $source ]]; then
  git clone --quiet --depth 1 --branch b11461 https://github.com/ggml-org/llama.cpp "$source"
fi
[[ $(git -C "$source" rev-parse HEAD) == "$revision" ]] || { echo "llama.cpp revision mismatch" >&2; exit 1; }
[[ -z $(git -C "$source" diff --name-only) ]] || { echo "llama.cpp source must be unmodified" >&2; exit 1; }
cuda=OFF vulkan=OFF
[[ ${GLIMPSE_BACKENDS:-cpu} != *cuda* ]] || cuda=ON
[[ ${GLIMPSE_BACKENDS:-cpu} != *vulkan* ]] || vulkan=ON
cmake -S "$source" -B "$build/llama-build" -DCMAKE_BUILD_TYPE=Release \
 -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON -DGGML_NATIVE=OFF \
 -DGGML_CPU_ALL_VARIANTS=ON -DGGML_CUDA="$cuda" -DGGML_VULKAN="$vulkan" \
 -DLLAMA_BUILD_MTMD=ON -DLLAMA_BUILD_COMMON=OFF -DLLAMA_BUILD_TOOLS=OFF \
 -DLLAMA_BUILD_APP=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_OPENSSL=OFF \
 -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF \
 -DCMAKE_INSTALL_RPATH='$ORIGIN' -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON >"$build/configure.log" 2>&1
cmake --build "$build/llama-build" -j "$jobs" >"$build/build.log" 2>&1
prefix="$build/prefix"
mkdir -p "$prefix/include" "$prefix/lib"
cp "$source/include/llama.h" "$source"/ggml/include/*.h "$source/tools/mtmd/mtmd.h" "$source/tools/mtmd/mtmd-helper.h" "$prefix/include/"
find "$build/llama-build" -name '*.so*' \( -type f -o -type l \) -exec cp -P {} "$prefix/lib/" \;
export GLIMPSE_LLAMA_DIR="$prefix"
cargo build --release --locked --manifest-path "$here/inference/Cargo.toml" -j "$jobs" --target-dir "$build/rust-target"
staging="$base/runtime.staging"
mkdir -p "$staging/bin" "$staging/lib"
install -m 755 "$build/rust-target/release/glimpse-embed" "$staging/bin/glimpse-embed"
cp -a "$prefix/lib/." "$staging/lib/"
cp "$source/LICENSE" "$staging/LLAMA_LICENSE"
printf '%s\n' "$revision" >"$staging/LLAMA_REVISION"
[[ ! -d "$base/runtime.previous" ]] || rm -rf "$base/runtime.previous"
[[ ! -d "$base/runtime" ]] || mv "$base/runtime" "$base/runtime.previous"
mv "$staging" "$base/runtime"
echo "Glimpse inference runtime ready. Models are downloaded separately."
