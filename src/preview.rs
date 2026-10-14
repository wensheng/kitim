//! Embedded preview mode (`--preview FILE COLS ROWS`): renders one file into
//! a cell area of another program's screen, such as a file manager's pane.
//!
//! The host program owns the terminal. kitim writes kitty graphics to stdout
//! and does no other terminal I/O: no raw mode, no queries, no input, no
//! audio. The host moves the cursor to the area's top-left cell and forwards
//! each frame unchanged, so kitim never needs to know where the area is.
//!
//! Each frame is an optional cursor move that centers the image, then one
//! chunked transmit-and-display command (see [`kitty::write_placed_frame`]).
//! Every frame gets a fresh image id, which the host uses to delete it once a
//! newer frame is up. The terminal scales each image to the frame's cell
//! rectangle, so animations are sent at reduced resolution.
//!
//! Still images print one frame and exit. Animated GIFs and videos loop,
//! muted, until the host kills kitim or closes the pipe. On failure kitim
//! prints a one-line message instead of frames and exits with status 1.

// Rust guideline compliant 2026-10-07

use crate::ffmpeg::{
    av_frame_alloc, av_frame_free, av_log_set_level, av_packet_alloc, av_packet_free,
    av_packet_unref, av_read_frame, avcodec_alloc_context3, avcodec_find_decoder,
    avcodec_free_context, avcodec_open2, avcodec_parameters_to_context, avcodec_receive_frame,
    avcodec_send_packet, avformat_close_input, avformat_find_stream_info, avformat_open_input,
    sws_freeContext, sws_getContext, sws_scale, AVCodecContext, AVFormatContext, AVFrame, AVPacket,
    AVStream, SwsContext, AVMEDIA_TYPE_VIDEO, AV_LOG_QUIET, AV_PIX_FMT_RGB24, SWS_BILINEAR,
};
use crate::kitty::{self, PixelFormat, Placed};
use crate::terminal_geometry::{cells_for_pixels, passive_cell_size};
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader};
use std::error::Error;
use std::ffi::{c_int, CString};
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Cell size assumed when neither `--cell-size` nor the tty driver gives one.
///
/// Matches kitim's full-screen default. A wrong guess only skews the size
/// and aspect ratio: the terminal still scales each image into its cells.
const DEFAULT_CELL: (u32, u32) = (10, 20);

/// Most pixels sent per frame of an animation or video, about 640x480.
///
/// The terminal scales frames up to fill their cells, so this trades
/// sharpness for bandwidth: full-size frames for a large pane can exceed
/// 10 MB each, which at 24 fps outruns both the pipe and the terminal.
const MAX_ANIMATION_PIXELS: u32 = 640 * 480;

/// How far behind schedule a video frame may be and still be shown.
///
/// Later frames are skipped, so slow decoding drops frames rather than
/// slowing the video down.
const LATE: Duration = Duration::from_millis(100);

/// Most video frames skipped in a row, so even slow decoding shows motion.
const MAX_SKIPS: u32 = 10;

/// Video frame duration when the stream declares no frame rate.
const DEFAULT_FRAME_TIME: Duration = Duration::from_millis(40);

/// Display time for GIF frames that declare none, as browsers do.
const DEFAULT_GIF_DELAY: Duration = Duration::from_millis(100);

/// Shortest GIF frame delay honored, also as browsers do.
const MIN_GIF_DELAY: Duration = Duration::from_millis(20);

/// Runs preview mode and returns the process exit status.
///
/// `args` holds `--preview`'s values: `FILE COLS ROWS`. `cell_size` is the
/// optional `--cell-size` value (`WxH` in pixels).
pub fn run(args: &[String], cell_size: Option<&str>) -> i32 {
    let mut out = io::stdout().lock();
    match preview(&mut out, args, cell_size) {
        Ok(()) => 0,
        // The host closed the pipe: it wants no more frames.
        Err(error) if is_broken_pipe(error.as_ref()) => 0,
        Err(error) => {
            let _ = writeln!(out, "{error}");
            let _ = out.flush();
            1
        }
    }
}

fn is_broken_pipe(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
}

fn preview(out: &mut impl Write, args: &[String], cell_size: Option<&str>) -> Result<()> {
    let [file, cols, rows] = args else {
        return Err("--preview takes FILE COLS ROWS".into());
    };
    let (cell_w, cell_h) = match cell_size {
        Some(text) => parse_size(text).ok_or("--cell-size takes WxH, e.g. 9x18")?,
        None => passive_cell_size().unwrap_or(DEFAULT_CELL),
    };
    let area = Area {
        cols: parse_count(cols)?,
        rows: parse_count(rows)?,
        cell_w,
        cell_h,
    };
    let format = ImageReader::open(file)?.with_guessed_format()?.format();
    match format {
        Some(ImageFormat::Gif) => play_gif(out, file, area),
        Some(_) => match show_image(out, file, area) {
            // A format the image crate knows but wasn't built to decode.
            Err(error) if is_unsupported(error.as_ref()) => play_video(out, file, area),
            result => result,
        },
        None => play_video(out, file, area),
    }
}

fn is_unsupported(error: &(dyn Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<ImageError>(),
        Some(ImageError::Unsupported(_))
    )
}

/// Parses a positive cell count.
fn parse_count(text: &str) -> Result<u32> {
    match text.parse::<u32>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("COLS and ROWS must be positive integers, got `{text}`").into()),
    }
}

/// Parses `WxH` with both parts positive.
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

// ------------------------------------------------------------------ layout

/// The cell area being previewed into, and the size of a cell in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    cols: u32,
    rows: u32,
    cell_w: u32,
    cell_h: u32,
}

/// Where and how large an image is shown inside an [`Area`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fit {
    /// Display size in pixels: the source size, shrunk to fit if needed.
    width: u32,
    height: u32,
    /// Cells the displayed image covers.
    cols: u32,
    rows: u32,
    /// Blank cells left of the image, centering it.
    left: u32,
}

impl Area {
    /// Fits a `width` x `height` image, shrinking it but never enlarging it.
    fn fit(self, width: u32, height: u32) -> Fit {
        let max_w = self.cols.saturating_mul(self.cell_w);
        let max_h = self.rows.saturating_mul(self.cell_h);
        let (width, height) = (width.max(1), height.max(1));
        let scale = (f64::from(max_w) / f64::from(width))
            .min(f64::from(max_h) / f64::from(height))
            .min(1.0);
        let width = scaled(width, scale).min(max_w);
        let height = scaled(height, scale).min(max_h);
        let cols = cells_for_pixels(width, self.cell_w);
        Fit {
            width,
            height,
            cols,
            rows: cells_for_pixels(height, self.cell_h),
            left: (self.cols - cols) / 2,
        }
    }
}

/// `pixels * scale`, rounded, at least 1.
fn scaled(pixels: u32, scale: f64) -> u32 {
    // Rounded and clamped to [1, u32::MAX], so the cast is exact.
    (f64::from(pixels) * scale)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32
}

/// Pixel size to send an animation frame at: `fit`'s size, capped at
/// [`MAX_ANIMATION_PIXELS`] with the aspect ratio kept.
fn animation_size(fit: Fit) -> (u32, u32) {
    let pixels = u64::from(fit.width) * u64::from(fit.height);
    if pixels <= u64::from(MAX_ANIMATION_PIXELS) {
        return (fit.width, fit.height);
    }
    // Pixel counts stay far below 2^52, so the conversion is exact.
    let scale = (f64::from(MAX_ANIMATION_PIXELS) / pixels as f64).sqrt();
    (scaled(fit.width, scale), scaled(fit.height, scale))
}

/// Writes one frame of `pixels` (`size` pixels) over `fit`'s cells.
fn emit(
    out: &mut impl Write,
    pixels: &[u8],
    format: PixelFormat,
    size: (u32, u32),
    fit: Fit,
) -> io::Result<()> {
    let frame = Placed {
        pixels,
        format,
        width_px: size.0,
        height_px: size.1,
        cols: fit.cols,
        rows: fit.rows,
        left: fit.left,
        id: kitty::next_image_id(),
    };
    kitty::write_placed_frame(out, &frame)?;
    out.flush()
}

// ------------------------------------------------------------------ images

/// Shows a still image, upright as its EXIF orientation says.
fn show_image(out: &mut impl Write, path: &str, area: Area) -> Result<()> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let fit = area.fit(image.width(), image.height());
    if (fit.width, fit.height) != (image.width(), image.height()) {
        image = image.resize_exact(fit.width, fit.height, FilterType::Triangle);
    }
    let rgba = image.into_rgba8();
    emit(
        out,
        rgba.as_raw(),
        PixelFormat::Rgba,
        (fit.width, fit.height),
        fit,
    )?;
    Ok(())
}

/// Plays a GIF in a loop; a single-frame GIF is shown once.
fn play_gif(out: &mut impl Write, path: &str, area: Area) -> Result<()> {
    loop {
        let decoder = GifDecoder::new(BufReader::new(File::open(path)?))?;
        let (width, height) = decoder.dimensions();
        let fit = area.fit(width, height);
        let size = animation_size(fit);
        let mut frames = decoder.into_frames().peekable();
        let mut shown = 0;
        while let Some(frame) = frames.next() {
            let frame = frame?;
            let delay = gif_delay(frame.delay().into());
            let mut pixels = frame.into_buffer();
            if pixels.dimensions() != size {
                pixels = image::imageops::resize(&pixels, size.0, size.1, FilterType::Triangle);
            }
            emit(out, pixels.as_raw(), PixelFormat::Rgba, size, fit)?;
            shown += 1;
            if frames.peek().is_some() {
                thread::sleep(delay);
            }
        }
        match shown {
            0 => return Err("GIF has no frames".into()),
            1 => return Ok(()),
            _ => {}
        }
    }
}

/// A GIF frame's display time, with browsers' fallbacks for tiny values.
fn gif_delay(declared: Duration) -> Duration {
    if declared.is_zero() {
        DEFAULT_GIF_DELAY
    } else {
        declared.max(MIN_GIF_DELAY)
    }
}

// ------------------------------------------------------------------ video

/// Plays a video's picture in a loop, in real time; a still is shown once.
fn play_video(out: &mut impl Write, path: &str, area: Area) -> Result<()> {
    let mut rgb = Vec::new();
    loop {
        let mut video = Video::open(path)?;
        let (width, height) = video.size()?;
        let fit = area.fit(width, height);
        let size = animation_size(fit);
        let frame_time = video.frame_time();
        let start = Instant::now();
        let mut first_pts = None;
        let mut last_at = Duration::ZERO;
        let (mut decoded, mut skips) = (0_u32, 0_u32);
        while video.decode() {
            let at = match video.pts_ms() {
                Some(pts) => {
                    Duration::from_millis(pts.saturating_sub(*first_pts.get_or_insert(pts)))
                }
                None => frame_time * decoded,
            };
            decoded += 1;
            last_at = at;
            let due = start + at;
            let now = Instant::now();
            if now > due + LATE && decoded > 1 && skips < MAX_SKIPS {
                skips += 1;
                continue;
            }
            skips = 0;
            thread::sleep(due.saturating_duration_since(now));
            video.scale(size, &mut rgb)?;
            emit(out, &rgb, PixelFormat::Rgb, size, fit)?;
        }
        match decoded {
            0 => return Err("no video frames".into()),
            1 => return Ok(()),
            _ => {}
        }
        // Give the last frame its time on screen before starting over.
        thread::sleep((start + last_at + frame_time).saturating_duration_since(Instant::now()));
    }
}

/// Decodes the first video stream of a file through kitim's `FFmpeg` bindings.
///
/// Owns every `FFmpeg` object it points to: [`Video::open`] creates them all
/// or fails, and `Drop` frees them. Raw pointers keep it off other threads.
struct Video {
    format: *mut AVFormatContext,
    codec: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    scaler: *mut SwsContext,
    /// Source width, height and pixel format, and target size, of `scaler`.
    scaler_for: Option<(c_int, c_int, c_int, (u32, u32))>,
    /// The video stream; `format` owns it.
    stream: *mut AVStream,
    index: c_int,
    /// Input ended: the decoder is handing out its delayed frames.
    draining: bool,
}

impl Video {
    fn open(path: &str) -> Result<Self> {
        let path = CString::new(path)?;
        let mut video = Self {
            format: ptr::null_mut(),
            codec: ptr::null_mut(),
            packet: ptr::null_mut(),
            frame: ptr::null_mut(),
            scaler: ptr::null_mut(),
            scaler_for: None,
            stream: ptr::null_mut(),
            index: -1,
            draining: false,
        };
        // SAFETY: `path` is NUL-terminated and outlives the calls. Every
        // pointer handed to FFmpeg was returned non-null by FFmpeg first, and
        // stream pointers are checked before use. `avformat_open_input` frees
        // its context and nulls `video.format` on failure. Everything
        // allocated is stored in `video` at once, so an early return frees
        // it through `Drop`.
        unsafe {
            av_log_set_level(AV_LOG_QUIET);
            if avformat_open_input(
                &raw mut video.format,
                path.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ) != 0
            {
                return Err("not an image or video".into());
            }
            if avformat_find_stream_info(video.format, ptr::null_mut()) < 0 {
                return Err("not an image or video".into());
            }
            let format = &*video.format;
            for i in 0..format.nb_streams {
                let stream = *format.streams.add(i as usize);
                if !stream.is_null()
                    && !(*stream).codecpar.is_null()
                    && (*(*stream).codecpar).codec_type == AVMEDIA_TYPE_VIDEO
                {
                    video.stream = stream;
                    video.index = c_int::try_from(i)?;
                    break;
                }
            }
            if video.stream.is_null() {
                return Err("no video stream".into());
            }
            let params = (*video.stream).codecpar;
            let decoder = avcodec_find_decoder((*params).codec_id);
            if decoder.is_null() {
                return Err("no decoder for this video".into());
            }
            video.codec = avcodec_alloc_context3(decoder);
            if video.codec.is_null()
                || avcodec_parameters_to_context(video.codec, params) < 0
                || avcodec_open2(video.codec, decoder, ptr::null_mut()) < 0
            {
                return Err("cannot open the video decoder".into());
            }
            video.packet = av_packet_alloc();
            video.frame = av_frame_alloc();
            if video.packet.is_null() || video.frame.is_null() {
                return Err("out of memory".into());
            }
        }
        Ok(video)
    }

    /// Picture size of the video stream in pixels.
    fn size(&self) -> Result<(u32, u32)> {
        // SAFETY: `open` checked `stream` and its `codecpar` are non-null;
        // both live as long as `format`, which `self` owns.
        let (width, height) = unsafe {
            let params = &*(*self.stream).codecpar;
            (params.width, params.height)
        };
        match (u32::try_from(width), u32::try_from(height)) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
            _ => Err("video has no picture size".into()),
        }
    }

    /// Frame duration from the stream's average frame rate.
    fn frame_time(&self) -> Duration {
        // SAFETY: `open` checked `stream` is non-null; `format` owns it.
        let rate = unsafe { (*self.stream).avg_frame_rate };
        match (u64::try_from(rate.num), u64::try_from(rate.den)) {
            (Ok(num), Ok(den)) if num > 0 && den > 0 => {
                Duration::from_micros(den.saturating_mul(1_000_000) / num)
            }
            _ => DEFAULT_FRAME_TIME,
        }
    }

    /// Decodes the next frame into `self.frame`; false at the end.
    ///
    /// Packets the decoder rejects are skipped, as players do. At the end of
    /// input the decoder is drained, so delayed frames are not lost.
    fn decode(&mut self) -> bool {
        loop {
            // SAFETY: `open` made `codec`, `frame`, `packet` and `format`
            // valid, and `self` owns them. A null packet is FFmpeg's
            // documented way to start draining.
            unsafe {
                if avcodec_receive_frame(self.codec, self.frame) == 0 {
                    return true;
                }
                if self.draining {
                    return false;
                }
                if av_read_frame(self.format, self.packet) < 0 {
                    avcodec_send_packet(self.codec, ptr::null());
                    self.draining = true;
                    continue;
                }
                if (*self.packet).stream_index == self.index {
                    avcodec_send_packet(self.codec, self.packet);
                }
                av_packet_unref(self.packet);
            }
        }
    }

    /// Presentation time of the decoded frame, in milliseconds.
    fn pts_ms(&self) -> Option<u64> {
        // SAFETY: `frame` and `stream` are valid for `self`'s lifetime.
        let (pts, base) = unsafe { ((*self.frame).pts, (*self.stream).time_base) };
        let (Ok(pts), Ok(num), Ok(den)) = (
            u64::try_from(pts),
            u64::try_from(base.num),
            u64::try_from(base.den),
        ) else {
            return None;
        };
        (num > 0 && den > 0).then(|| pts.saturating_mul(num).saturating_mul(1000) / den)
    }

    /// Scales the decoded frame to `size` pixels of packed RGB in `rgb`.
    fn scale(&mut self, size: (u32, u32), rgb: &mut Vec<u8>) -> Result<()> {
        let (width, height) = (c_int::try_from(size.0)?, c_int::try_from(size.1)?);
        let stride = usize::try_from(size.0)? * 3;
        let len = stride * usize::try_from(size.1)?;
        // SAFETY: `decode` just filled `frame`, so its planes and strides
        // describe a `width` x `height` picture in `format`, and the scaler
        // is rebuilt whenever those change. Both pointer arrays hold the four
        // entries `sws_scale` reads. The output holds `height` rows of
        // `stride` bytes, plus slack for SIMD stores past the end.
        unsafe {
            let frame = &*self.frame;
            let key = (frame.width, frame.height, frame.format, size);
            if frame.width <= 0 || frame.height <= 0 {
                return Err("video frame has no picture".into());
            }
            if self.scaler_for != Some(key) {
                sws_freeContext(self.scaler);
                self.scaler_for = None;
                self.scaler = sws_getContext(
                    frame.width,
                    frame.height,
                    frame.format,
                    width,
                    height,
                    AV_PIX_FMT_RGB24,
                    SWS_BILINEAR,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                );
                if self.scaler.is_null() {
                    return Err("cannot convert this video's pixel format".into());
                }
                self.scaler_for = Some(key);
            }
            rgb.clear();
            rgb.resize(len + 64, 0);
            let planes = [
                rgb.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let strides = [c_int::try_from(stride)?, 0, 0, 0];
            let rows = sws_scale(
                self.scaler,
                frame.data.as_ptr().cast(),
                frame.linesize.as_ptr(),
                0,
                frame.height,
                planes.as_ptr(),
                strides.as_ptr(),
            );
            if rows <= 0 {
                return Err("video frame conversion failed".into());
            }
        }
        rgb.truncate(len);
        Ok(())
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by `self`, and freed exactly
        // once here. Every free function accepts null (or a pointer to null).
        unsafe {
            sws_freeContext(self.scaler);
            av_frame_free(&raw mut self.frame);
            av_packet_free(&raw mut self.packet);
            avcodec_free_context(&raw mut self.codec);
            avformat_close_input(&raw mut self.format);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Area = Area {
        cols: 40,
        rows: 10,
        cell_w: 10,
        cell_h: 20,
    };

    #[test]
    fn small_images_keep_their_size_and_are_centered() {
        let fit = AREA.fit(95, 41);
        assert_eq!((fit.width, fit.height, fit.cols, fit.rows), (95, 41, 10, 3));
        assert_eq!(fit.left, 15);
    }

    #[test]
    fn large_images_shrink_into_the_area_keeping_aspect() {
        // 400x200 px area; a 1600x400 image is width-bound at a quarter.
        let fit = AREA.fit(1600, 400);
        assert_eq!(
            (fit.width, fit.height, fit.cols, fit.rows, fit.left),
            (400, 100, 40, 5, 0)
        );
        let fit = AREA.fit(300, 3000);
        assert_eq!((fit.width, fit.height), (20, 200));
        assert!(fit.cols <= AREA.cols && fit.rows <= AREA.rows);
    }

    #[test]
    fn animations_are_capped_in_pixels_not_cells() {
        let fit = Area {
            cols: 200,
            rows: 60,
            cell_w: 20,
            cell_h: 40,
        }
        .fit(3840, 2160);
        let (w, h) = animation_size(fit);
        assert!(w * h <= MAX_ANIMATION_PIXELS + w + h, "{w}x{h}");
        assert!((w * 9).abs_diff(h * 16) <= 16, "aspect kept: {w}x{h}");
        assert_eq!(animation_size(AREA.fit(64, 48)), (64, 48));
    }

    #[test]
    fn sizes_and_counts_parse_strictly() {
        assert_eq!(parse_size("9x18"), Some((9, 18)));
        assert_eq!(parse_size("9x0"), None);
        assert_eq!(parse_size("9"), None);
        assert_eq!(parse_count("12").unwrap(), 12);
        assert!(parse_count("0").is_err());
        assert!(parse_count("-3").is_err());
    }

    #[test]
    fn gif_delays_follow_browser_rules() {
        assert_eq!(gif_delay(Duration::ZERO), DEFAULT_GIF_DELAY);
        assert_eq!(gif_delay(Duration::from_millis(10)), MIN_GIF_DELAY);
        assert_eq!(
            gif_delay(Duration::from_millis(70)),
            Duration::from_millis(70)
        );
    }
}
