use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{env, fmt, fs};

fn flag_if_supported_with_fallbacks(config: &mut cc::Build, flags: &[&str]) {
    let option = flags
        .iter()
        .find(|flag| config.is_flag_supported(flag).unwrap_or_default());

    if let Some(flag) = option {
        config.flag(flag);
    }
}

fn compile_zstd() {
    let mut config = cc::Build::new();

    // Search the following directories for C files to add to the compilation.
    for dir in &[
        "zstd/lib/common",
        "zstd/lib/compress",
        "zstd/lib/decompress",
    ] {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(Result::unwrap)
            .filter_map(|entry| {
                let filename = entry.file_name();

                if Path::new(&filename).extension() == Some(OsStr::new("c"))
                    // Skip xxhash*.c files: since we are using the "PRIVATE API"
                    // mode, it will be inlined in the headers.
                    && !filename.to_string_lossy().contains("xxhash")
                {
                    Some(entry.path())
                } else {
                    None
                }
            })
            .collect();
        entries.sort();

        config.files(entries);
    }

    // Either include ASM files, or disable ASM entirely.
    //
    // The only assembly zstd ships is huf_decompress_amd64.S, which upstream
    // gates on __x86_64__ in portability_macros.h. Anywhere else it can only
    // ever preprocess to an empty object, while still costing an assembler
    // invocation.
    //
    // Also disable it on windows, apparently it doesn't do well with these .S files at the moment.
    let target_arch =
        std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if cfg!(feature = "no_asm")
        || target_arch != "x86_64"
        || std::env::var("CARGO_CFG_WINDOWS").is_ok()
    {
        config.define("ZSTD_DISABLE_ASM", Some(""));
    } else {
        config.file("zstd/lib/decompress/huf_decompress_amd64.S");
    }

    // List out the WASM targets that need wasm-shim.
    // Note that Emscripten already provides its own C standard library so
    // wasm32-unknown-emscripten should not be included here.
    // See: https://github.com/gyscos/zstd-rs/pull/209
    let need_wasm_shim = !cfg!(feature = "no_wasm_shim")
        && env::var("TARGET").map_or(false, |target| {
            target == "wasm32-unknown-unknown"
                || target.starts_with("wasm32-wasi")
        });

    if need_wasm_shim {
        cargo_print(&"rerun-if-changed=wasm-shim/stdlib.h");
        cargo_print(&"rerun-if-changed=wasm-shim/string.h");

        config.include("wasm-shim/");
    }

    // Some extra parameters
    config.include("zstd/lib/");
    config.include("zstd/lib/common");
    config.warnings(false);

    config.define("ZSTD_LIB_DEPRECATED", Some("0"));

    config
        .flag_if_supported("-ffunction-sections")
        .flag_if_supported("-fdata-sections")
        .flag_if_supported("-fmerge-all-constants");

    if cfg!(feature = "fat-lto") {
        config.flag_if_supported("-flto");
    } else if cfg!(feature = "thin-lto") {
        flag_if_supported_with_fallbacks(
            &mut config,
            &["-flto=thin", "-flto"],
        );
    }

    // lzcb: this fork ships no legacy sources.
    config.define("ZSTD_LEGACY_SUPPORT", Some("0"));


    // lzcb: rename every exported C symbol so this fork can be linked into the same
    // binary as the genuine zstd-sys. Regenerate lzcb_prefix.h with
    // ../../tools/gen-symbol-prefix.sh after any upstream bump.
    println!("cargo:rerun-if-changed=lzcb_prefix.h");
    println!("cargo:rerun-if-env-changed=LZCB_NO_PREFIX");
    if env::var_os("LZCB_NO_PREFIX").is_none() {
        let hdr = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("lzcb_prefix.h");
        assert!(
            hdr.exists(),
            "lzcb_prefix.h is missing; run tools/gen-symbol-prefix.sh"
        );
        let hdr = hdr.to_str().expect("non-UTF-8 manifest dir");
        // Never flag_if_supported here: a silently dropped flag brings the symbol
        // collision back with no diagnostic. The joined -includeFILE form is a single
        // argv element, so it can never be separated from its argument.
        let msvc = config.try_get_compiler().map(|c| c.is_like_msvc()).unwrap_or(false);
        if msvc {
            config.flag(format!("/FI{hdr}"));
        } else {
            config.flag(format!("-include{hdr}"));
        }
    }

    // lzcb: so the `#include "lzcb_prefix.h"` added to zstd_common.c resolves.
    config.include(env::current_dir().unwrap());

    // Hide symbols from resulting library,
    // so we can be used with another zstd-linking lib.
    // See https://github.com/gyscos/zstd-rs/issues/58
    if !target_is_msvc() {
        config.flag("-fvisibility=hidden");
    }
    config.define("XXH_PRIVATE_API", Some(""));
    config.define("ZSTDLIB_VISIBILITY", Some(""));
    config.define("ZSTDERRORLIB_VISIBILITY", Some(""));

    // https://github.com/facebook/zstd/blob/d69d08ed6c83563b57d98132e1e3f2487880781e/lib/common/debug.h#L60
    /* recommended values for DEBUGLEVEL :
     * 0 : release mode, no debug, all run-time checks disabled
     * 1 : enables assert() only, no display
     * 2 : reserved, for currently active debug path
     * 3 : events once per object lifetime (CCtx, CDict, etc.)
     * 4 : events once per frame
     * 5 : events once per block
     * 6 : events once per sequence (verbose)
     * 7+: events at every position (*very* verbose)
     */
    // Compile!
    // lzcb: not libzstd.a. The archive's directory is on the final link's search path, where it
    // would otherwise stand in for a system libzstd that a crate links with -lzstd.
    config.compile("liblzcb_zstd.a");

    let src = env::current_dir().unwrap().join("zstd").join("lib");
    let dst = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let include = dst.join("include");
    fs::create_dir_all(&include).unwrap();
    fs::copy(src.join("zstd.h"), include.join("zstd.h")).unwrap();
    fs::copy(src.join("zstd_errors.h"), include.join("zstd_errors.h"))
        .unwrap();
    cargo_print(&format_args!("root={}", dst.display()));
}

/// Is the *target* toolchain MSVC? `cfg!(target_env = ..)` would answer for the host.
fn target_is_msvc() -> bool {
    env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc"
}

/// Print a line for cargo.
fn cargo_print(content: &dyn fmt::Display) {
    println!("cargo:{}", content);
}

fn main() {
    // ggcat: the vendored decoder carries the LZ match hook patch.
    println!("cargo:rerun-if-changed=zstd/lib/decompress/zstd_decompress.c");
    println!("cargo:rerun-if-changed=zstd/lib/decompress/zstd_decompress_block.c");
    println!("cargo:rerun-if-changed=zstd/lib/decompress/zstd_decompress_internal.h");
    println!("cargo:rerun-if-changed=zstd/lib/common/zstd_common.c");

    let target_arch =
        std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    if target_arch == "wasm32" || target_os == "hermit" {
        cargo_print(&"rustc-cfg=feature=\"std\"");
    }

    // println!("cargo:rustc-link-lib=zstd");
    // lzcb: this fork carries the LZ match hook, so the bundled source is always
    // built; a system libzstd would lack ZSTD_DCtx_setLzMatchHook. Upstream's
    // pkg-config path is deliberately gone.
    if !Path::new("zstd/lib").exists() {
        panic!("Folder 'zstd/lib' does not exist.");
    }
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("Manifest dir is always set by cargo"),
    );
    compile_zstd();
    let headerpaths = vec![manifest_dir.join("zstd/lib")];

    let includes: Vec<_> = headerpaths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    cargo_print(&format_args!("include={}", includes.join(";")));
}
