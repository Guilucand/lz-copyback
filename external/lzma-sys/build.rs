use std::env;
use std::fs;
use std::path::PathBuf;

const SKIP_FILENAMES: &[&str] = &["crc32_small", "crc64_small", "crc_clmul_consts_gen"];

fn main() {
    let target = env::var("TARGET").unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LZMA_API_STATIC");
    println!("cargo:rerun-if-changed=xz/src/liblzma/lz/lz_decoder.c");
    println!("cargo:rerun-if-changed=xz/src/liblzma/lz/lz_decoder.h");
    println!("cargo:rerun-if-changed=xz/src/liblzma/lzma/lzma_decoder.c");
    println!("cargo:rerun-if-changed=xz/src/liblzma/common/block_decoder.c");
    // lzcb: this fork carries the LZ match hook, so the bundled source is always
    // built; a system liblzma would lack lzma_lz_set_match_hook. Upstream's
    // pkg-config probe is deliberately gone.
    let want_parallel = false;

    let out_dir = env::var("OUT_DIR").unwrap();
    println!("cargo:root={}", out_dir);
    let include_dir = env::current_dir().unwrap().join("xz/src/liblzma/api");
    println!("cargo:include={}", include_dir.display());

    let mut src_files = [
        "xz/src/liblzma/common",
        "xz/src/liblzma/lzma",
        "xz/src/liblzma/lz",
        "xz/src/liblzma/check",
        "xz/src/liblzma/delta",
        "xz/src/liblzma/rangecoder",
        "xz/src/liblzma/simple",
    ]
    .iter()
    .flat_map(|dir| read_dir_files(dir))
    .chain(vec![
        "xz/src/common/tuklib_cpucores.c".into(),
        "xz/src/common/tuklib_physmem.c".into(),
    ])
    .collect::<Vec<_>>();

    if !want_parallel {
        src_files = src_files
            .into_iter()
            .filter(|path| !path.file_stem().unwrap().to_str().unwrap().ends_with("mt"))
            .collect::<Vec<_>>();
    }

    // sort to make build reproducible.
    src_files.sort();

    let mut build = cc::Build::new();

    if !cfg!(debug_assertions) {
        build.define("NDEBUG", None);
    }

    build
        .files(src_files)
        // all C preproc defines are in `./config.h`
        .define("HAVE_CONFIG_H", "1")
        .include("xz/src/liblzma/api")
        .include("xz/src/liblzma/lzma")
        .include("xz/src/liblzma/lz")
        .include("xz/src/liblzma/check")
        .include("xz/src/liblzma/simple")
        .include("xz/src/liblzma/delta")
        .include("xz/src/liblzma/common")
        .include("xz/src/liblzma/rangecoder")
        .include("xz/src/common")
        .include(env::current_dir().unwrap());


    // lzcb: rename every exported C symbol so this fork can be linked into the same
    // binary as the genuine liblzma-sys. Regenerate lzcb_prefix.h with
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
        let msvc = build.try_get_compiler().map(|c| c.is_like_msvc()).unwrap_or(false);
        if msvc {
            build.flag(format!("/FI{hdr}"));
        } else {
            build.flag(format!("-include{hdr}"));
        }
    }

    // lzcb: tuklib_* cannot be renamed with a #define -- tuklib_common.h pastes the
    // name with ## via TUKLIB_SYMBOL(), which blocks macro expansion. Use upstream's
    // own prefix hook instead, and keep tuklib_* out of lzcb_prefix.h.
    build.define("TUKLIB_SYMBOL_PREFIX", "lzcb_");

    if !target.ends_with("msvc") {
        build.flag("-std=c99");
        // lzcb: upstream never marks anything default-visible (config.h does not define
        // HAVE_VISIBILITY), so this costs nothing and keeps the lzma_* symbols out of
        // any cdylib built on top.
        build.flag("-fvisibility=hidden");
        if want_parallel {
            build.flag("-pthread");
        }
        // ggcat: runtime-detected CLMUL CRC32/CRC64 (see PATCH.md). Without it the
        // table-driven CRC64 costs as much as the LZMA decoding itself on highly
        // repetitive input.
        if target.starts_with("x86_64") {
            build.define("HAVE_USABLE_CLMUL", "1");
        }
    }
    if cfg!(feature = "fat-lto") {
        build.flag_if_supported("-flto");
    } else if cfg!(feature = "thin-lto") {
        // find the first flag in `flags` that is supported and add that to.
        let flags = ["-flto=thin", "-flto"];
        let option = flags
            .into_iter()
            .find(|flag| build.is_flag_supported(flag).unwrap_or_default());
        if let Some(flag) = option {
            build.flag(flag);
        }
    }
    if want_parallel {
        build.define("LZMA_SYS_ENABLE_THREADS", "1");
    }

    if let Ok(s) = env::var("CARGO_CFG_TARGET_ENDIAN") {
        if s == "big" {
            build.define("WORDS_BIGENDIAN", None);
        }
    }

    // List out the WASM targets that need wasm-shim.
    // Note that Emscripten already provides its own C standard library so
    // wasm32-unknown-emscripten should not be included here.
    let need_wasm_shim = target == "wasm32-unknown-unknown" || target.starts_with("wasm32-wasi");
    if need_wasm_shim {
        println!("cargo:rerun-if-changed=wasm-shim/assert.h");
        println!("cargo:rerun-if-changed=wasm-shim/stdlib.h");
        println!("cargo:rerun-if-changed=wasm-shim/string.h");
        build.include("wasm-shim/");
    }

    build.compile("liblzma.a");
}

fn read_dir_files(dir: &str) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(dir)
        .unwrap_or_else(|_| panic!("failed to read dir {}", dir))
        .filter_map(|ent| {
            let ent = ent.expect("failed to read entry");

            if ent.file_type().unwrap().is_dir() {
                return None;
            }

            let path = ent.path();

            if path.extension().unwrap() != "c" {
                return None;
            }

            {
                let file_stem = path.file_stem().unwrap().to_str().unwrap();
                if SKIP_FILENAMES.contains(&file_stem) {
                    return None;
                }
                if file_stem.ends_with("tablegen") {
                    return None;
                }
            }

            Some(path)
        })
}
