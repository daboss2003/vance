// Minimal `extern "C"` bridge over the OpenColorIO C++ API (Phase 6.1b).
//
// OCIO 2.x has no stable C API, so Rust can't FFI it directly. This shim
// exposes the one operation Vance needs — bake a `src -> dst` colorspace
// conversion into a 3D LUT — behind a C ABI. The Rust side generates an
// identity lattice (the cube's sample coordinates) and hands it here; we
// run OCIO's CPU processor over it IN PLACE, turning it into the baked
// LUT. All C++ exceptions are caught at the boundary (unwinding across
// the FFI edge is UB) and reported via a return code + error string.

#include <OpenColorIO/OpenColorIO.h>

#include <cstring>

namespace OCIO = OCIO_NAMESPACE;

extern "C" {

// Copy `msg` into the caller's error buffer, NUL-terminated and bounded.
static void vance_ocio_set_err(char *errbuf, long errbuf_len, const char *msg) {
    if (!errbuf || errbuf_len <= 0) {
        return;
    }
    std::strncpy(errbuf, msg, static_cast<size_t>(errbuf_len) - 1);
    errbuf[errbuf_len - 1] = '\0';
}

// Returns 0 on success. On failure returns nonzero and writes a message
// into `errbuf`. `config_path` may be null/empty to use the `$OCIO`
// environment config. `rgb` is `num_pixels * 3` floats, transformed in
// place.
int vance_ocio_bake_lut(const char *config_path, const char *src_cs,
                        const char *dst_cs, float *rgb, long num_pixels,
                        char *errbuf, long errbuf_len) {
    try {
        if (!rgb || num_pixels <= 0 || !src_cs || !dst_cs) {
            vance_ocio_set_err(errbuf, errbuf_len, "null/empty argument");
            return 3;
        }
        OCIO::ConstConfigRcPtr config;
        if (config_path && config_path[0] != '\0') {
            config = OCIO::Config::CreateFromFile(config_path);
        } else {
            config = OCIO::Config::CreateFromEnv();
        }
        if (!config) {
            vance_ocio_set_err(errbuf, errbuf_len, "failed to load OCIO config");
            return 4;
        }
        OCIO::ConstProcessorRcPtr processor = config->getProcessor(src_cs, dst_cs);
        OCIO::ConstCPUProcessorRcPtr cpu = processor->getDefaultCPUProcessor();
        // Treat the lattice as a 1-row packed RGB image.
        OCIO::PackedImageDesc desc(rgb, num_pixels, 1, 3);
        cpu->apply(desc);
        return 0;
    } catch (const OCIO::Exception &e) {
        vance_ocio_set_err(errbuf, errbuf_len, e.what());
        return 1;
    } catch (const std::exception &e) {
        vance_ocio_set_err(errbuf, errbuf_len, e.what());
        return 2;
    } catch (...) {
        vance_ocio_set_err(errbuf, errbuf_len, "unknown OCIO error");
        return 2;
    }
}

// List the config's colorspaces, names joined by '\n' into `namebuf`.
// Returns the count on success, or -1 on error (message in `errbuf`).
// If `namebuf` is too small the names are truncated but the full count
// is still returned, so the caller can retry with a bigger buffer.
int vance_ocio_list_colorspaces(const char *config_path, char *namebuf,
                                long namebuf_len, char *errbuf,
                                long errbuf_len) {
    try {
        OCIO::ConstConfigRcPtr config;
        if (config_path && config_path[0] != '\0') {
            config = OCIO::Config::CreateFromFile(config_path);
        } else {
            config = OCIO::Config::CreateFromEnv();
        }
        if (!config) {
            vance_ocio_set_err(errbuf, errbuf_len, "failed to load OCIO config");
            return -1;
        }
        int count = config->getNumColorSpaces();
        if (namebuf && namebuf_len > 0) {
            std::string joined;
            for (int i = 0; i < count; ++i) {
                if (i) {
                    joined += '\n';
                }
                joined += config->getColorSpaceNameByIndex(i);
            }
            std::strncpy(namebuf, joined.c_str(), static_cast<size_t>(namebuf_len) - 1);
            namebuf[namebuf_len - 1] = '\0';
        }
        return count;
    } catch (const OCIO::Exception &e) {
        vance_ocio_set_err(errbuf, errbuf_len, e.what());
        return -1;
    } catch (const std::exception &e) {
        vance_ocio_set_err(errbuf, errbuf_len, e.what());
        return -1;
    } catch (...) {
        vance_ocio_set_err(errbuf, errbuf_len, "unknown OCIO error");
        return -1;
    }
}

// Linked-library version string (e.g. "2.5.2"). Also a cheap smoke test
// that the native library linked correctly.
const char *vance_ocio_version() { return OCIO::GetVersion(); }

} // extern "C"
