//! Embedded preview mode (`--preview FILE COLS ROWS`): renders one file into
//! a cell area of another program's screen, such as a file manager's pane.
//!
//! The host program owns the terminal. kitim writes kitty graphics to stdout
//! and does no other terminal I/O: no raw mode, no queries, no input. The
//! host moves the cursor to the area's top-left cell and forwards each frame
//! unchanged, so kitim never needs to know where the area is.
//!
//! Each frame is an optional cursor move that centers the image, then one
//! chunked transmit-and-display command (see [`kitty::write_placed_frame`]).
//! Every frame gets a fresh image id, which the host uses to delete it once a
//! newer frame is up. The terminal scales each image to the frame's cell
//! rectangle, so animations are sent at reduced resolution.
//!
//! Still images print one frame and exit. Animated GIFs and videos loop
//! until the host kills kitim or closes the pipe. Videos are muted unless
//! `--sound` is given; then their audio plays on the default output device
//! and paces the picture, as in kitim's player. On failure kitim prints a
//! one-line message instead of frames and exits with status 1.

// Rust guideline compliant 2026-10-07

use crate::ffmpeg::{
    av_channel_layout_copy, av_channel_layout_default, av_channel_layout_uninit, av_frame_alloc,
    av_frame_free, av_log_set_level, av_packet_alloc, av_packet_free, av_packet_unref,
    av_read_frame, avcodec_alloc_context3, avcodec_find_decoder, avcodec_free_context,
    avcodec_open2, avcodec_parameters_to_context, avcodec_receive_frame, avcodec_send_packet,
    avformat_close_input, avformat_find_stream_info, avformat_open_input, swr_alloc_set_opts2,
    swr_convert, swr_free, swr_init, sws_freeContext, sws_getContext, sws_scale, AVChannelLayout,
    AVCodecContext, AVCodecParameters, AVFormatContext, AVFrame, AVPacket, AVStream, SwrContext,
    SwsContext, AVMEDIA_TYPE_AUDIO, AVMEDIA_TYPE_VIDEO, AV_LOG_QUIET, AV_PIX_FMT_RGB24,
    AV_SAMPLE_FMT_FLT, SWS_BILINEAR,
};
use crate::kitty::{self, PixelFormat, Placed};
use crate::terminal_geometry::{cells_for_pixels, passive_cell_size};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};
use std::collections::VecDeque;
use std::error::Error;
use std::ffi::{c_int, CString};
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// Audio decoded ahead of the picture while sound plays.
///
/// Files interleave audio and video in chunks, so the audio for a moment
/// can sit after later video in the file. Decoding this far ahead keeps the
/// device fed; frames decoded meanwhile wait in a queue.
const AUDIO_AHEAD: Duration = Duration::from_millis(500);

/// Most frames queued while decoding ahead: about 30 MB at 640x480.
///
/// Files interleaved more coarsely than this starve the audio briefly
/// instead of using more memory.
const MAX_QUEUE: usize = 32;

/// How long the audio clock may stand still before time runs on without it.
///
/// Longer than an output callback's period, so the clock's normal stepping
/// never counts as stopped. When the audio track ends before the picture,
/// the video pauses this long, then keeps going.
const AUDIO_STALL: Duration = Duration::from_millis(100);

/// Longest wait between decoding steps while sound plays, to keep audio fed.
const AUDIO_NAP: Duration = Duration::from_millis(10);

/// Seconds of samples the speaker's ring holds; far above [`AUDIO_AHEAD`],
/// so pushing only waits if the device has stopped taking samples.
const RING_SECONDS: usize = 2;

/// How long a push may wait on a full ring before dropping the rest.
const RING_WAIT: Duration = Duration::from_millis(500);

/// Frames of room for each read of the resampler's leftovers at the end.
const FLUSH_ROOM: c_int = 1024;

/// Runs preview mode and returns the process exit status.
///
/// `args` holds `--preview`'s values: `FILE COLS ROWS`. `cell_size` is the
/// optional `--cell-size` value (`WxH` in pixels). `sound` plays videos'
/// audio.
pub fn run(args: &[String], cell_size: Option<&str>, sound: bool) -> i32 {
    let mut out = io::stdout().lock();
    match preview(&mut out, args, cell_size, sound) {
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

fn preview(
    out: &mut impl Write,
    args: &[String],
    cell_size: Option<&str>,
    sound: bool,
) -> Result<()> {
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
            Err(error) if is_unsupported(error.as_ref()) => play_video(out, file, area, sound),
            result => result,
        },
        None => play_video(out, file, area, sound),
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

/// Plays a video in a loop, in real time; a silent still is shown once.
///
/// With `sound`, its audio plays too, if it has any and a device exists.
fn play_video(out: &mut impl Write, path: &str, area: Area, sound: bool) -> Result<()> {
    // Opening the device can take a few hundred ms; show the picture first.
    if sound {
        show_first_frame(out, path, area)?;
    }
    // Kept across loops: reopening the device would gap the sound.
    let mut speaker = if sound { Speaker::open() } else { None };
    let mut spare = Vec::new();
    loop {
        let mut video = Video::open(path, speaker.as_ref().map(Speaker::format))?;
        let (width, height) = video.size()?;
        let fit = area.fit(width, height);
        let audio = speaker.as_mut().filter(|_| video.has_audio());
        let silent = audio.is_none();
        match play_pass(out, &mut video, fit, audio, &mut spare)? {
            0 => return Err("no video frames".into()),
            1 if silent => return Ok(()),
            _ => {}
        }
    }
}

/// Shows a video's first frame, if it has one.
fn show_first_frame(out: &mut impl Write, path: &str, area: Area) -> Result<()> {
    let mut video = Video::open(path, None)?;
    let (width, height) = video.size()?;
    let fit = area.fit(width, height);
    let size = animation_size(fit);
    let mut samples = Vec::new();
    loop {
        match video.step(&mut samples) {
            Step::Picture => break,
            Step::End => return Ok(()),
            Step::Sound | Step::Idle => {}
        }
    }
    let mut rgb = Vec::new();
    video.scale(size, &mut rgb)?;
    emit(out, &rgb, PixelFormat::Rgb, size, fit)?;
    Ok(())
}

/// Plays `video` once through, and returns how many frames it decoded.
///
/// Decodes ahead into a queue: one frame when silent, or with sound enough
/// for [`AUDIO_AHEAD`] of audio. Each frame waits for the [`Clock`], and is
/// skipped if it is more than [`LATE`]. Ends once the last frame and all
/// queued sound have played. `spare` recycles frame buffers.
fn play_pass(
    out: &mut impl Write,
    video: &mut Video,
    fit: Fit,
    mut audio: Option<&mut Speaker>,
    spare: &mut Vec<Vec<u8>>,
) -> Result<u32> {
    let size = animation_size(fit);
    let frame_time = video.frame_time();
    if let Some(speaker) = audio.as_deref_mut() {
        speaker.restart();
    }
    let mut clock = Clock::new(Instant::now());
    let mut queue: VecDeque<(Duration, Vec<u8>)> = VecDeque::new();
    let mut samples = Vec::new();
    let mut first_pts = None;
    let (mut decoded, mut skips, mut shown, mut ended) = (0_u32, 0_u32, false, false);
    let mut last_at = Duration::ZERO;
    loop {
        while !ended
            && queue.len() < MAX_QUEUE
            && (queue.is_empty() || audio.as_ref().is_some_and(|s| s.buffered() < AUDIO_AHEAD))
        {
            match video.step(&mut samples) {
                Step::Picture => {
                    let at = match video.pts_ms() {
                        Some(pts) => {
                            Duration::from_millis(pts.saturating_sub(*first_pts.get_or_insert(pts)))
                        }
                        None => frame_time * decoded,
                    };
                    decoded += 1;
                    let mut rgb = spare.pop().unwrap_or_default();
                    video.scale(size, &mut rgb)?;
                    queue.push_back((at, rgb));
                }
                Step::Sound => {
                    if let Some(speaker) = audio.as_deref_mut() {
                        speaker.push(&samples);
                    }
                }
                Step::Idle => {}
                Step::End => ended = true,
            }
        }
        let Some(&(at, _)) = queue.front() else { break };
        let now = clock.now(audio.as_ref().map(|s| s.played()), Instant::now());
        if at > now {
            let wait = at.saturating_sub(now);
            thread::sleep(if audio.is_some() {
                wait.min(AUDIO_NAP)
            } else {
                wait
            });
            continue;
        }
        let (at, rgb) = queue.pop_front().expect("front was just checked");
        if now > at + LATE && shown && skips < MAX_SKIPS {
            skips += 1;
        } else {
            skips = 0;
            shown = true;
            last_at = at;
            emit(out, &rgb, PixelFormat::Rgb, size, fit)?;
        }
        spare.push(rgb);
    }
    // Let the last frame, and any sound still queued, play out.
    let end = (last_at + frame_time).max(audio.as_ref().map_or(Duration::ZERO, |s| s.pushed()));
    loop {
        let now = clock.now(audio.as_ref().map(|s| s.played()), Instant::now());
        if now >= end {
            return Ok(decoded);
        }
        thread::sleep(end.saturating_sub(now).min(AUDIO_NAP));
    }
}

/// Media time that paces frames: the audio's position while sound plays,
/// else wall-clock time since the pass started.
#[derive(Debug)]
struct Clock {
    start: Instant,
    /// Last audio position seen, and when it last moved.
    heard: Duration,
    moved: Instant,
}

impl Clock {
    fn new(start: Instant) -> Self {
        Self {
            start,
            heard: Duration::ZERO,
            moved: start,
        }
    }

    /// Media time at `now`; `played` is how much sound the device has
    /// taken, or `None` when silent.
    fn now(&mut self, played: Option<Duration>, now: Instant) -> Duration {
        let Some(played) = played else {
            return now.saturating_duration_since(self.start);
        };
        if played != self.heard {
            self.heard = played;
            self.moved = now;
        }
        // Sound keeps time while it flows. Once it stops (its track ended,
        // or decoding fell behind), time runs on from where it stopped.
        played
            + now
                .saturating_duration_since(self.moved)
                .saturating_sub(AUDIO_STALL)
    }
}

/// The default audio output device, fed through a ring of samples.
struct Speaker {
    /// Plays while it lives.
    _stream: cpal::Stream,
    ring: HeapProd<f32>,
    /// Samples the device has taken from the ring.
    played: Arc<AtomicU64>,
    /// Samples put in the ring this pass.
    pushed: u64,
    channels: u16,
    rate: u32,
}

impl Speaker {
    /// Opens the default output; `None` if there is none, so videos play
    /// silently instead of failing.
    fn open() -> Option<Self> {
        let device = cpal::default_host().default_output_device()?;
        let config: cpal::StreamConfig = device.default_output_config().ok()?.into();
        let (channels, rate) = (config.channels, config.sample_rate);
        if channels == 0 || rate == 0 {
            return None;
        }
        let capacity = usize::try_from(rate).ok()? * usize::from(channels) * RING_SECONDS;
        let (ring, mut source) = HeapRb::<f32>::new(capacity).split();
        let played = Arc::new(AtomicU64::new(0));
        let taken = Arc::clone(&played);
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let n = source.pop_slice(data);
                    data[n..].fill(0.0);
                    taken.fetch_add(n as u64, Ordering::Relaxed);
                },
                |_| {},
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(Self {
            _stream: stream,
            ring,
            played,
            pushed: 0,
            channels,
            rate,
        })
    }

    /// Channel count and sample rate that decoded audio is converted to.
    fn format(&self) -> (u16, u32) {
        (self.channels, self.rate)
    }

    /// Starts counting a new pass; the last one waited for its sound to end.
    fn restart(&mut self) {
        self.played.store(0, Ordering::Relaxed);
        self.pushed = 0;
    }

    /// Queues interleaved samples. Waits while the ring is full, and drops
    /// what is left after [`RING_WAIT`]: the device has stopped.
    fn push(&mut self, mut samples: &[f32]) {
        let deadline = Instant::now() + RING_WAIT;
        while !samples.is_empty() && Instant::now() < deadline {
            let n = self.ring.push_slice(samples);
            self.pushed += n as u64;
            samples = &samples[n..];
            if n == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Sound the device has taken this pass.
    fn played(&self) -> Duration {
        self.duration(self.played.load(Ordering::Relaxed))
    }

    /// Sound queued this pass.
    fn pushed(&self) -> Duration {
        self.duration(self.pushed)
    }

    /// Sound queued but not yet taken by the device.
    fn buffered(&self) -> Duration {
        self.duration(
            self.pushed
                .saturating_sub(self.played.load(Ordering::Relaxed)),
        )
    }

    fn duration(&self, samples: u64) -> Duration {
        let per_second = u64::from(self.channels) * u64::from(self.rate);
        Duration::from_micros(samples.saturating_mul(1_000_000) / per_second)
    }
}

/// What one [`Video::step`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A picture is decoded; scale it before the next step.
    Picture,
    /// Samples are ready in the step's buffer.
    Sound,
    /// A packet was consumed with nothing to show yet.
    Idle,
    /// The file is fully decoded.
    End,
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
    /// The first audio stream's decoder, when sound was asked for.
    audio: Option<Audio>,
    /// Input ended: the decoders are handing out their delayed frames.
    draining: bool,
}

impl Video {
    /// Opens `path`; with `sound` (channels, sample rate), also its audio.
    fn open(path: &str, sound: Option<(u16, u32)>) -> Result<Self> {
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
            audio: None,
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
            if let Some((channels, rate)) = sound {
                video.audio = Audio::open(&*video.format, channels, rate);
            }
        }
        Ok(video)
    }

    /// Whether the file has audio that `open` could decode.
    fn has_audio(&self) -> bool {
        self.audio.is_some()
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

    /// Decodes one step: a picture into `self.frame`, or audio converted
    /// into `samples`, or just one packet read.
    ///
    /// Packets a decoder rejects are skipped, as players do. At the end of
    /// input the decoders are drained, so delayed frames are not lost. Audio
    /// that can't be converted turns the sound off rather than failing.
    fn step(&mut self, samples: &mut Vec<f32>) -> Step {
        // SAFETY: `open` made `codec`, `frame`, `packet` and `format` valid,
        // and `self` owns them; so does `audio` for its own decoder. A null
        // packet is FFmpeg's documented way to start draining.
        unsafe {
            if avcodec_receive_frame(self.codec, self.frame) == 0 {
                return Step::Picture;
            }
            if let Some(audio) = self.audio.as_mut() {
                if avcodec_receive_frame(audio.codec, audio.frame) == 0 {
                    if audio.convert(samples) {
                        return Step::Sound;
                    }
                    self.audio = None;
                    return Step::Idle;
                }
            }
            if self.draining {
                let flushed = self.audio.as_mut().is_some_and(|a| a.flush(samples));
                return if flushed { Step::Sound } else { Step::End };
            }
            if av_read_frame(self.format, self.packet) < 0 {
                avcodec_send_packet(self.codec, ptr::null());
                if let Some(audio) = &self.audio {
                    avcodec_send_packet(audio.codec, ptr::null());
                }
                self.draining = true;
                return Step::Idle;
            }
            let index = (*self.packet).stream_index;
            if index == self.index {
                avcodec_send_packet(self.codec, self.packet);
            } else if let Some(audio) = self.audio.as_ref().filter(|a| a.index == index) {
                avcodec_send_packet(audio.codec, self.packet);
            }
            av_packet_unref(self.packet);
        }
        Step::Idle
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
        // Before `format`, which owns the stream parameters it points to.
        self.audio = None;
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

/// Decodes an audio stream and converts it for the [`Speaker`].
///
/// Owns its decoder, frame and resampler, freed in `Drop`. `params` belongs
/// to the format context of the [`Video`] that owns this, which drops it
/// first.
struct Audio {
    codec: *mut AVCodecContext,
    frame: *mut AVFrame,
    /// Created for the first frame's format; null until then.
    resampler: *mut SwrContext,
    params: *mut AVCodecParameters,
    index: c_int,
    channels: u16,
    rate: u32,
    /// The resampler's last samples were taken at the end of input.
    flushed: bool,
}

impl Audio {
    /// Opens the first audio stream of `format`, to convert to `channels` at
    /// `rate`; `None` if there is no stream or it can't be decoded.
    ///
    /// # Safety
    ///
    /// `format` must be a context `avformat_open_input` and
    /// `avformat_find_stream_info` succeeded on, and must outlive the result.
    unsafe fn open(format: &AVFormatContext, channels: u16, rate: u32) -> Option<Self> {
        let mut audio = Self {
            codec: ptr::null_mut(),
            frame: ptr::null_mut(),
            resampler: ptr::null_mut(),
            params: ptr::null_mut(),
            index: -1,
            channels,
            rate,
            flushed: false,
        };
        // SAFETY: the caller vouches for `format`; stream and parameter
        // pointers are checked before use. Everything allocated is stored in
        // `audio` at once, so an early return frees it through `Drop`.
        unsafe {
            for i in 0..format.nb_streams {
                let stream = *format.streams.add(i as usize);
                if !stream.is_null()
                    && !(*stream).codecpar.is_null()
                    && (*(*stream).codecpar).codec_type == AVMEDIA_TYPE_AUDIO
                {
                    audio.params = (*stream).codecpar;
                    audio.index = c_int::try_from(i).ok()?;
                    break;
                }
            }
            if audio.params.is_null() {
                return None;
            }
            let decoder = avcodec_find_decoder((*audio.params).codec_id);
            if decoder.is_null() {
                return None;
            }
            audio.codec = avcodec_alloc_context3(decoder);
            if audio.codec.is_null()
                || avcodec_parameters_to_context(audio.codec, audio.params) < 0
                || avcodec_open2(audio.codec, decoder, ptr::null_mut()) < 0
            {
                return None;
            }
            audio.frame = av_frame_alloc();
        }
        (!audio.frame.is_null()).then_some(audio)
    }

    /// Converts the decoded frame into interleaved `f32` samples in `out`.
    /// Returns false if it can't be converted.
    fn convert(&mut self, out: &mut Vec<f32>) -> bool {
        out.clear();
        // SAFETY: `frame` was just filled by the decoder and `params` lives
        // as long as `self` (see the type's docs). The layouts are freed
        // after `swr_alloc_set_opts2`, which copies them. `out` holds room
        // for `room` frames of `channels` samples, as `swr_convert` is told.
        unsafe {
            let frame = &*self.frame;
            let in_rate = if frame.sample_rate > 0 {
                frame.sample_rate
            } else {
                (*self.params).sample_rate
            };
            if in_rate <= 0 || frame.nb_samples <= 0 {
                return true; // nothing to play
            }
            let Ok(rate) = c_int::try_from(self.rate) else {
                return false;
            };
            if self.resampler.is_null() {
                let mut in_layout = AVChannelLayout::default();
                if (*self.params).ch_layout.nb_channels > 0 {
                    av_channel_layout_copy(&raw mut in_layout, &raw const (*self.params).ch_layout);
                } else {
                    av_channel_layout_default(&raw mut in_layout, 2);
                }
                let mut out_layout = AVChannelLayout::default();
                av_channel_layout_default(&raw mut out_layout, c_int::from(self.channels));
                let made = swr_alloc_set_opts2(
                    &raw mut self.resampler,
                    &raw const out_layout,
                    AV_SAMPLE_FMT_FLT,
                    rate,
                    &raw const in_layout,
                    frame.format,
                    in_rate,
                    0,
                    ptr::null_mut(),
                );
                av_channel_layout_uninit(&raw mut in_layout);
                av_channel_layout_uninit(&raw mut out_layout);
                if made < 0 || self.resampler.is_null() || swr_init(self.resampler) < 0 {
                    swr_free(&raw mut self.resampler);
                    return false;
                }
            }
            let room = i64::from(frame.nb_samples) * i64::from(rate) / i64::from(in_rate) + 256;
            let Ok(room) = c_int::try_from(room) else {
                return false;
            };
            let channels = usize::from(self.channels);
            out.resize(usize::try_from(room).unwrap_or(0) * channels, 0.0);
            let mut planes = [out.as_mut_ptr().cast::<u8>(); 1];
            let input = if frame.extended_data.is_null() {
                frame.data.as_ptr().cast::<*const u8>()
            } else {
                frame.extended_data.cast_const().cast::<*const u8>()
            };
            let converted = swr_convert(
                self.resampler,
                planes.as_mut_ptr(),
                room,
                input,
                frame.nb_samples,
            );
            let Ok(converted) = usize::try_from(converted) else {
                out.clear();
                return false;
            };
            out.truncate(converted * channels);
        }
        true
    }

    /// Takes samples still held by the resampler at the end of input.
    /// Returns false once there are none.
    fn flush(&mut self, out: &mut Vec<f32>) -> bool {
        out.clear();
        if self.flushed || self.resampler.is_null() {
            return false;
        }
        let channels = usize::from(self.channels);
        out.resize(FLUSH_ROOM as usize * channels, 0.0);
        // SAFETY: `resampler` is initialized, and `out` holds `FLUSH_ROOM` frames.
        // A null input with a zero count asks for buffered samples.
        let converted = unsafe {
            let mut planes = [out.as_mut_ptr().cast::<u8>(); 1];
            swr_convert(
                self.resampler,
                planes.as_mut_ptr(),
                FLUSH_ROOM,
                ptr::null(),
                0,
            )
        };
        match usize::try_from(converted) {
            Ok(n) if n > 0 => {
                out.truncate(n * channels);
                true
            }
            _ => {
                self.flushed = true;
                out.clear();
                false
            }
        }
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned by `self`, freed once here.
        // `params` belongs to the format context and is not freed.
        unsafe {
            swr_free(&raw mut self.resampler);
            av_frame_free(&raw mut self.frame);
            avcodec_free_context(&raw mut self.codec);
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
    fn silent_clock_is_wall_time_since_the_pass_started() {
        let t0 = Instant::now();
        let mut clock = Clock::new(t0);
        assert_eq!(
            clock.now(None, t0 + Duration::from_millis(250)),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn audio_clock_follows_sound_and_runs_on_when_it_stops() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut clock = Clock::new(t0);
        // Flowing sound is the time, whatever the wall clock says.
        assert_eq!(clock.now(Some(ms(30)), t0 + ms(40)), ms(30));
        assert_eq!(clock.now(Some(ms(60)), t0 + ms(70)), ms(60));
        // Standing still for less than AUDIO_STALL is normal stepping.
        assert_eq!(clock.now(Some(ms(60)), t0 + ms(70) + AUDIO_STALL), ms(60));
        // After that, time runs on from where the sound stopped.
        assert_eq!(clock.now(Some(ms(60)), t0 + ms(170) + AUDIO_STALL), ms(160));
        // When sound comes back, it is the time again.
        assert_eq!(clock.now(Some(ms(90)), t0 + ms(400)), ms(90));
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
