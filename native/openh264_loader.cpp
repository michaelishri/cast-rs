#include <codec_api.h>

#include <dlfcn.h>
#include <cstdlib>
#include <mutex>
#include <string>
#include <vector>

namespace {
std::once_flag load_once;
void* module = nullptr;

using VersionFunction = void (*)(OpenH264Version*);

bool compatible(void* candidate) {
  auto version_function = reinterpret_cast<VersionFunction>(
      dlsym(candidate, "WelsGetCodecVersionEx"));
  if (version_function == nullptr) {
    return false;
  }
  OpenH264Version version{};
  version_function(&version);
  return version.uMajor == 2 && version.uMinor == 3;
}

void try_load(const std::string& path) {
  if (module != nullptr || path.empty()) {
    return;
  }
  void* candidate = dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (candidate == nullptr) {
    return;
  }
  if (compatible(candidate)) {
    module = candidate;
  } else {
    dlclose(candidate);
  }
}

void load_module() {
  if (const char* override_path = std::getenv("CAST_OPENH264_LIBRARY")) {
    try_load(override_path);
  }
  for (const char* path : {
           "/usr/lib/libopenh264.so.6",
           "/usr/lib/libopenh264.so.7",
           "/usr/lib/libopenh264.so",
           "/usr/local/lib/libopenh264.so.6",
           "/usr/local/lib/libopenh264.so.7",
           "/usr/local/lib/libopenh264.so",
           "/lib/libopenh264.so.6",
           "/lib/libopenh264.so.7",
           "/lib/libopenh264.so",
           "/usr/lib/x86_64-linux-gnu/libopenh264.so.6",
           "/usr/lib/x86_64-linux-gnu/libopenh264.so.7",
           "/usr/lib/x86_64-linux-gnu/libopenh264.so",
           "/lib/x86_64-linux-gnu/libopenh264.so.6",
           "/lib/x86_64-linux-gnu/libopenh264.so.7",
           "/lib/x86_64-linux-gnu/libopenh264.so",
           "/usr/lib/aarch64-linux-gnu/libopenh264.so.6",
           "/usr/lib/aarch64-linux-gnu/libopenh264.so.7",
           "/usr/lib/aarch64-linux-gnu/libopenh264.so",
           "/lib/aarch64-linux-gnu/libopenh264.so.6",
           "/lib/aarch64-linux-gnu/libopenh264.so.7",
           "/lib/aarch64-linux-gnu/libopenh264.so",
       }) {
    try_load(path);
  }
  if (const char* data_home = std::getenv("XDG_DATA_HOME")) {
    try_load(std::string(data_home) + "/cast/codecs/libopenh264.so.6");
  } else if (const char* home = std::getenv("HOME")) {
    try_load(std::string(home) + "/.local/share/cast/codecs/libopenh264.so.6");
  }
}

template <typename Function>
Function symbol(const char* name) {
  std::call_once(load_once, load_module);
  if (module == nullptr) {
    return nullptr;
  }
  return reinterpret_cast<Function>(dlsym(module, name));
}
}  // namespace

extern "C" int WelsCreateSVCEncoder(ISVCEncoder** encoder) {
  using Function = int (*)(ISVCEncoder**);
  auto function = symbol<Function>("WelsCreateSVCEncoder");
  return function == nullptr ? 1 : function(encoder);
}

extern "C" void WelsDestroySVCEncoder(ISVCEncoder* encoder) {
  using Function = void (*)(ISVCEncoder*);
  if (auto function = symbol<Function>("WelsDestroySVCEncoder")) {
    function(encoder);
  }
}

extern "C" long WelsCreateDecoder(ISVCDecoder** decoder) {
  using Function = long (*)(ISVCDecoder**);
  auto function = symbol<Function>("WelsCreateDecoder");
  return function == nullptr ? 1 : function(decoder);
}

extern "C" void WelsDestroyDecoder(ISVCDecoder* decoder) {
  using Function = void (*)(ISVCDecoder*);
  if (auto function = symbol<Function>("WelsDestroyDecoder")) {
    function(decoder);
  }
}

extern "C" OpenH264Version WelsGetCodecVersion() {
  using Function = OpenH264Version (*)();
  auto function = symbol<Function>("WelsGetCodecVersion");
  return function == nullptr ? OpenH264Version{} : function();
}

extern "C" void WelsGetCodecVersionEx(OpenH264Version* version) {
  using Function = void (*)(OpenH264Version*);
  auto function = symbol<Function>("WelsGetCodecVersionEx");
  if (function == nullptr) {
    *version = OpenH264Version{};
  } else {
    function(version);
  }
}
