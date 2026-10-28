//! Builds the Go Live codec shims, the GIF thumbnail encoder, the video file
//! reader and the cameras against the libvpx, FFmpeg, dav1d, libyuv and
//! WebRTC headers in the same libwebrtc archive webrtc-sys links, so they
//! match its code, and the H264 encoder against x264 (vendor/x264-src). On
//! macOS it also builds the process tap's Objective-C part, for Go Live's
//! sound, and what asks macOS about screen capture.

fn main() {
    for file in [
        "vp8.c",
        "h264.c",
        "clip.c",
        "decode.c",
        "video.c",
        "picture.c",
        "camera.cc",
        "camera_mac.m",
        "tap_mac.m",
        "capture_mac.c",
        "hwenc.c",
        "hevc.c",
        "vtdec.c",
        "vtenc.c",
        "vtdec.h",
        "aacdec.c",
        "aacdec.h",
    ] {
        println!("cargo:rerun-if-changed=native/{file}");
    }
    let include = webrtc_sys_build::webrtc_dir().join("include");
    let third_party = include.join("third_party");
    cc::Build::new()
        .file("native/picture.c")
        .include(third_party.join("libyuv/include"))
        .warnings(true)
        .compile("yapfast_picture");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // Linux captures through V4L2, and Windows through DirectShow, both in
    // the same module of the libwebrtc archive.
    if matches!(target_os.as_str(), "linux" | "windows") {
        let mut camera = cc::Build::new();
        camera
            .cpp(true)
            .file("native/camera.cc")
            .include(&include)
            .include(third_party.join("abseil-cpp"))
            .include(third_party.join("libyuv/include"))
            .std("c++20")
            .flag("-Wno-deprecated-declarations")
            // libwebrtc's headers mark some pointers and not others.
            .flag("-Wno-nullability-completeness")
            .warnings(true);
        // The toolchain sets its own hardening mode.
        for (key, value) in webrtc_sys_build::webrtc_defines() {
            if key != "_LIBCPP_HARDENING_MODE" {
                camera.define(&key, value.as_deref());
            }
        }
        camera.compile("yapfast_camera");
        // DirectShow's device list reads each camera's name and path as a
        // VARIANT; webrtc-sys links the rest.
        if target_os == "windows" {
            println!("cargo:rustc-link-lib=dylib=oleaut32");
        }
    }
    // macOS films through AVFoundation, with camera.cc's interface, and
    // libyuv from the same archive.
    if target_os == "macos" {
        cc::Build::new()
            .file("native/camera_mac.m")
            .include(third_party.join("libyuv/include"))
            .flag("-fobjc-arc")
            .warnings(true)
            .compile("yapfast_camera");
        for framework in ["AVFoundation", "CoreMedia", "CoreVideo", "Foundation"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
        // libwebrtc's Objective-C categories load only with -ObjC, which
        // webrtc-sys passes for its own targets alone; without it this
        // crate's tests abort as LiveKit makes its first peer connection
        // factory (+[RTCVideoEncoderVP9 scalabilityModes]).
        println!("cargo:rustc-link-arg=-ObjC");
    }
    println!("cargo::rustc-check-cfg=cfg(hwenc)");
    println!("cargo::rustc-check-cfg=cfg(hevcdec)");
    if std::env::var_os("CARGO_FEATURE_DISCORD").is_none() {
        return;
    }
    // libwebrtc's macOS FFmpeg decodes neither H264 nor H265, and
    // ffmpeg-src builds no encoders there, so VideoToolbox decodes and
    // encodes: decode.c's and video.c's H264, vtdec.c's H265 in place of
    // hevc.c's, and vtenc.c in place of hwenc.c. Nor AAC, which AudioToolbox
    // decodes for video.c, in aacdec.c. webrtc-sys links the frameworks
    // already; they're named for these files' own calls.
    let videotoolbox = target_os == "macos";
    // FFmpeg's hardware encoders and HEVC decoder, where ffmpeg-src builds
    // them: hwenc.c and hevc.c see only their headers, with FFmpeg's names
    // prefixed as in the library.
    if !videotoolbox && std::env::var("DEP_FFMPEG_ENCODERS").as_deref() == Ok("1") {
        let mut ffmpeg = cc::Build::new();
        ffmpeg
            .file("native/hwenc.c")
            .file("native/hevc.c")
            .warnings(true);
        let folders = std::env::var_os("DEP_FFMPEG_INCLUDE").expect("ffmpeg-src's headers");
        ffmpeg.includes(std::env::split_paths(&folders));
        let rename = std::env::var("DEP_FFMPEG_RENAME").expect("ffmpeg-src's renames");
        // clang-cl, for Windows, spells the flags as MSVC does.
        if ffmpeg.get_compiler().is_like_msvc() {
            ffmpeg.flag("/std:c17").flag(format!("/FI{rename}"));
        } else {
            ffmpeg.flag("-std=c17").flag("-include").flag(&rename);
        }
        ffmpeg.compile("yapfast_hwenc");
        println!("cargo:rustc-cfg=hwenc");
        println!("cargo:rustc-cfg=hevcdec");
    }
    // Go Live's sound on macOS: a process tap, whose description is an
    // Objective-C object.
    if target_os == "macos" {
        cc::Build::new()
            .file("native/tap_mac.m")
            .flag("-fobjc-arc")
            .warnings(true)
            .compile("yapfast_tap");
        // Screen capture's permission, and Yapfast's own windows, which
        // the share menu leaves out.
        cc::Build::new()
            .file("native/capture_mac.c")
            .warnings(true)
            .compile("yapfast_capture");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
    }
    if videotoolbox {
        println!("cargo:rustc-cfg=hwenc");
        println!("cargo:rustc-cfg=hevcdec");
        for framework in [
            "VideoToolbox",
            "AudioToolbox",
            "CoreMedia",
            "CoreVideo",
            "CoreFoundation",
        ] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
    }
    let os = match target_os.as_str() {
        "windows" => "win",
        "macos" => "mac",
        _ => "linux",
    };
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        _ => "x64",
    };
    let mut codecs = cc::Build::new();
    if videotoolbox {
        codecs
            .file("native/vtdec.c")
            .file("native/vtenc.c")
            .file("native/aacdec.c");
    }
    codecs
        .file("native/vp8.c")
        .file("native/h264.c")
        .file("native/clip.c")
        .file("native/decode.c")
        .file("native/video.c")
        .include(third_party.join("libvpx/source/libvpx"))
        // x264-src's build script hands over its headers' folder.
        .include(std::env::var_os("DEP_X264_INCLUDE").expect("x264-src's headers"))
        .include(third_party.join("ffmpeg"))
        .include(third_party.join("dav1d/libdav1d/include"))
        .include(third_party.join("libyuv/include"))
        // FFmpeg's configuration for the Chrome build libwebrtc uses.
        .include(third_party.join(format!("ffmpeg/chromium/config/Chrome/{os}/{arch}")))
        .warnings(true)
        .compile("yapfast_codecs");
}
