use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(ffmpeg_old_channel_layout)");
    for variable in [
        "FFMPEG_DIR",
        "VCPKG_ROOT",
        "VCPKG_TARGET_TRIPLET",
        "VCPKG_DEFAULT_TRIPLET",
        "PKG_CONFIG_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        configure_windows();
        return;
    }

    // FF_API_OLD_CHANNEL_LAYOUT is gated on libavutil major < 59 (i.e. FFmpeg 6.x and
    // older). With it active the AVCodecParameters and AVFrame layouts include the
    // deprecated channel_layout/channels (plus several other) fields, so we need a
    // different Rust mirror of those structs. FFmpeg 7.x removed them and shifted the
    // remaining fields.
    if let Some(major) = pkg_config_major("libavutil") {
        if major < 59 {
            println!("cargo:rustc-cfg=ffmpeg_old_channel_layout");
        }
    }

    // Prefer pkg-config — it picks up the right library path and link names on both
    // Linux distros and Homebrew-on-macOS without hardcoding paths.
    if emit_pkg_config_libs() {
        return;
    }

    // Fallback: hand-rolled paths for environments without pkg-config (e.g. macOS where
    // brew's pkg-config isn't on PATH). Keep the original Homebrew hints.
    if target.contains("apple-darwin") {
        println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
        println!("cargo:rustc-link-search=native=/usr/local/lib");
    }
    for lib in ["avcodec", "avformat", "avutil", "swscale", "swresample"] {
        println!("cargo:rustc-link-lib={}", lib);
    }
}

fn configure_windows() {
    let sdk = std::env::var_os("FFMPEG_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let root = std::env::var_os("VCPKG_ROOT").expect(
            "Windows builds require a shared FFmpeg SDK: set FFMPEG_DIR, or install ffmpeg with vcpkg and set VCPKG_ROOT",
        );
        let triplet = std::env::var("VCPKG_TARGET_TRIPLET")
            .or_else(|_| std::env::var("VCPKG_DEFAULT_TRIPLET"))
            .unwrap_or_else(|_| {
                match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default().as_str() {
                    "x86_64" => "x64-windows",
                    "aarch64" => "arm64-windows",
                    "x86" => "x86-windows",
                    architecture => panic!("unsupported Windows architecture: {architecture}"),
                }.to_owned()
            });
        PathBuf::from(root).join("installed").join(triplet)
    });
    let header = sdk.join("include/libavutil/version.h");
    println!("cargo:rerun-if-changed={}", header.display());
    let version = std::fs::read_to_string(&header).unwrap_or_else(|error| {
        panic!(
            "cannot read FFmpeg SDK header {}: {error}",
            header.display()
        )
    });
    let major = version
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == "#define" && words.next()? == "LIBAVUTIL_VERSION_MAJOR")
                .then(|| words.next()?.parse::<u32>().ok())
                .flatten()
        })
        .expect("FFmpeg SDK header is missing LIBAVUTIL_VERSION_MAJOR");
    if major < 59 {
        println!("cargo:rustc-cfg=ffmpeg_old_channel_layout");
    }
    let libraries = sdk.join("lib");
    println!("cargo:rustc-link-search=native={}", libraries.display());
    for library in ["avcodec", "avformat", "avutil", "swscale", "swresample"] {
        assert!(
            libraries.join(format!("{library}.lib")).is_file(),
            "FFmpeg SDK is missing {library}.lib in {}",
            libraries.display()
        );
        println!("cargo:rustc-link-lib=dylib={library}");
    }

    // Stage the shared SDK runtime and its codec dependencies for direct launches and tests.
    // OUT_DIR is <profile>/build/<package>/out, including cross-target builds.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));
    let profile = out.ancestors().nth(3).expect("invalid Cargo OUT_DIR");
    let runtime = sdk.join("bin");
    println!("cargo:rerun-if-changed={}", runtime.display());
    for entry in std::fs::read_dir(&runtime).expect("FFmpeg SDK requires a bin directory") {
        let source = entry.expect("cannot read FFmpeg runtime entry").path();
        if source
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dll"))
        {
            println!("cargo:rerun-if-changed={}", source.display());
            for destination in [profile.to_path_buf(), profile.join("deps")] {
                std::fs::create_dir_all(&destination)
                    .expect("cannot create Cargo output directory");
                std::fs::copy(
                    &source,
                    destination.join(source.file_name().expect("DLL has a name")),
                )
                .expect("cannot stage FFmpeg runtime DLL");
            }
        }
    }
}

fn emit_pkg_config_libs() -> bool {
    let output = Command::new("pkg-config")
        .args([
            "--libs",
            "libavformat",
            "libavcodec",
            "libavutil",
            "libswscale",
            "libswresample",
        ])
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for arg in stdout.split_whitespace() {
        if let Some(path) = arg.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={}", path);
        } else if let Some(name) = arg.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={}", name);
        }
    }
    true
}

fn pkg_config_major(lib: &str) -> Option<u32> {
    let output = Command::new("pkg-config")
        .args(["--modversion", lib])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    version.split('.').next()?.trim().parse().ok()
}
