// Rust guideline compliant 2026-10-07
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn built_executable_starts_with_native_runtime() {
    // Starting the linked binary also exercises Windows DLL discovery before main.
    let output = Command::new(env!("CARGO_BIN_EXE_kitim"))
        .arg("--help")
        .output()
        .expect("kitim should launch");
    assert!(
        output.status.success(),
        "kitim exited with {}",
        output.status
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kitim-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn preview(file: &Path, cols: &str, rows: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kitim"));
    command
        .arg("--preview")
        .arg(file)
        .args([cols, rows, "--cell-size", "10x20"]);
    command
}

/// Image commands (`ESC _ G a=T ...`) in `out`, as their control data.
fn frames(out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    text.match_indices("\x1b_Ga=T,")
        .map(|(at, _)| {
            let rest = &text[at + 3..];
            rest[..rest.find(';').unwrap()].to_string()
        })
        .collect()
}

/// Reads `command`'s stdout until it has `want` frames or 10 s pass.
fn first_frames(mut command: Command, want: usize) -> Vec<String> {
    let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (mut out, mut buf) = (Vec::new(), vec![0; 1 << 16]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while frames(&out).len() < want && Instant::now() < deadline {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    frames(&out)
}

#[test]
fn preview_still_image_is_one_centered_quiet_frame() {
    let dir = scratch("still");
    let file = dir.join("dot.png");
    image::RgbaImage::new(30, 20).save(&file).unwrap();

    let output = preview(&file, "10", "5").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    // 30x20 px is 3x1 cells, centered in 10 columns.
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("\x1b[3C\x1b_Ga=T,i="),
        "{:?}",
        &text[..40.min(text.len())]
    );
    let frames = frames(text.as_bytes());
    assert_eq!(frames.len(), 1);
    assert!(
        frames[0].ends_with(",f=32,s=30,v=20,c=3,r=1,C=1,q=2,m=0"),
        "{}",
        frames[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_animated_gif_loops_with_fresh_image_ids() {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, RgbaImage};

    let dir = scratch("gif");
    let file = dir.join("blink.gif");
    let mut encoder = GifEncoder::new(std::fs::File::create(&file).unwrap());
    for shade in [0, 255] {
        let pixels = RgbaImage::from_pixel(8, 8, image::Rgba([shade, shade, shade, 255]));
        let delay = Delay::from_numer_denom_ms(20, 1);
        encoder
            .encode_frame(Frame::from_parts(pixels, 0, 0, delay))
            .unwrap();
    }
    drop(encoder);

    // Two frames, so a third proves it loops.
    let frames = first_frames(preview(&file, "10", "5"), 3);
    assert_eq!(frames.len(), 3, "{frames:?}");
    let ids: Vec<&str> = frames
        .iter()
        .map(|f| f.split(',').nth(1).unwrap())
        .collect();
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2],
        "ids must differ: {ids:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_video_plays_rgb_frames_when_ffmpeg_is_available() {
    let dir = scratch("video");
    let file = dir.join("clip.mp4");
    let made = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc=size=64x48:rate=10")
        .args(["-t", "1", "-c:v", "mpeg4"])
        .arg(&file)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipped: the ffmpeg CLI is needed to make a test video");
        return;
    }
    let frames = first_frames(preview(&file, "20", "10"), 3);
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert!(
        frames[0].contains(",f=24,s=64,v=48,c=7,r=3,"),
        "{}",
        frames[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_with_sound_plays_video_with_an_audio_track() {
    let dir = scratch("sound");
    let file = dir.join("hush.mp4");
    // Silent audio: the test may play through a real device.
    let made = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc=size=64x48:rate=10")
        .args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo"])
        .args(["-t", "1", "-c:v", "mpeg4", "-c:a", "aac", "-shortest"])
        .arg(&file)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("skipped: the ffmpeg CLI is needed to make a test video");
        return;
    }
    // Without an audio device the video plays silently; either way it plays.
    let mut command = preview(&file, "20", "10");
    command.arg("--sound");
    let frames = first_frames(command, 3);
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert!(frames[0].contains(",f=24,s=64,v=48,"), "{}", frames[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_options_need_preview_mode() {
    for option in [&["--sound"][..], &["--cell-size", "9x18"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_kitim"))
            .args(option)
            .arg("a.mp4")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{option:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("only work with --preview"), "{stderr}");
    }
}

#[test]
fn preview_failures_print_one_line_and_exit_1() {
    let dir = scratch("fail");
    let file = dir.join("notes.dat");
    std::fs::write(&file, "hello world\n").unwrap();

    let output = preview(&file, "10", "5").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "not an image or video\n"
    );

    let output = preview(&file, "10", "0").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("positive integers"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preview_conflicts_with_playback_files() {
    let output = Command::new(env!("CARGO_BIN_EXE_kitim"))
        .args(["a.png", "--preview", "b.png", "10", "5"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}
