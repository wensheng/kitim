use crate::config::Config;
use crate::ffmpeg::*;
use crate::kitty;
use crate::playback_control;
use crate::terminal_geometry::{cells_for_pixels, TerminalGeometry};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::Print,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::ffi::{c_int, CString};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// CPAL and Ringbuf imports
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STATUS_SHORTCUTS: &str = "f +10s b -10s Left -5s Right +5s Up/Down vol g goto q quit";
const AVSEEK_FLAG_BACKWARD: c_int = 1;
const AV_NOPTS_VALUE: i64 = i64::MIN;

#[derive(Debug)]
enum RenderMessage {
    Frame {
        rgba: Vec<u8>,
        pts_ms: u64,
        generation: u64,
        display_size: VideoDisplaySize,
    },
    Layout(VideoTerminalLayout),
    Reset {
        generation: u64,
    },
    RedrawStatus,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlayerCommand {
    SeekBy(i64),
    SeekTo(u64),
    VolumeBy(i32),
    Resize(TerminalGeometry),
    Quit,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct VideoDisplaySize {
    target_cols: u32,
    target_rows: u32,
    target_w_px: u32,
    target_h_px: u32,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct VideoTerminalLayout {
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    video_area_rows: u16,
    status_row: Option<u16>,
}

impl VideoTerminalLayout {
    fn current() -> Self {
        Self::from_geometry(TerminalGeometry::current())
    }

    fn from_geometry(geometry: TerminalGeometry) -> Self {
        let cols = geometry.cols.max(1);
        let rows = geometry.rows.max(1);
        let status_row = (rows >= 2).then_some(rows - 1);
        let video_area_rows = if status_row.is_some() { rows - 1 } else { rows }.max(1);

        Self {
            cols,
            rows,
            cell_width_px: geometry.cell_width_px,
            cell_height_px: geometry.cell_height_px,
            video_area_rows,
            status_row,
        }
    }

    fn origin_for(self, display_size: VideoDisplaySize) -> (u16, u16) {
        let target_cols = display_size.target_cols.min(u32::from(self.cols)) as u16;
        let target_rows = display_size
            .target_rows
            .min(u32::from(self.video_area_rows)) as u16;
        let origin_col = self.cols.saturating_sub(target_cols) / 2;
        let origin_row = self.video_area_rows.saturating_sub(target_rows) / 2;
        (origin_col, origin_row)
    }
}

#[derive(Debug)]
struct PlaybackState {
    terminal_layout: VideoTerminalLayout,
    display_size: VideoDisplaySize,
    duration_ms: Option<u64>,
    volume_percent: u32,
    generation: u64,
    drop_until_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct StatusState {
    current_ms: u64,
    duration_ms: Option<u64>,
    volume_percent: u32,
    message: String,
    input_buffer: Option<String>,
}

impl Default for StatusState {
    fn default() -> Self {
        Self {
            current_ms: 0,
            duration_ms: None,
            volume_percent: 100,
            message: String::new(),
            input_buffer: None,
        }
    }
}

type SharedStatus = Arc<Mutex<StatusState>>;

#[derive(Debug, Default)]
struct ControlInputState {
    goto_buffer: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct KeyOutcome {
    commands: Vec<PlayerCommand>,
    message: Option<String>,
    redraw_status: bool,
}

struct VideoTerminalSession;

impl VideoTerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(
            stdout,
            EnterAlternateScreen,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            let _ = disable_raw_mode();
            return Err(err);
        }
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for VideoTerminalSession {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            Show,
            Clear(ClearType::All),
            LeaveAlternateScreen,
            MoveTo(0, 0)
        );
        let _ = stdout.flush();
        let _ = disable_raw_mode();
    }
}

fn target_pixels_for_source_dimension(source_px: c_int, zoom: f32) -> u32 {
    let source_px = source_px.max(1) as f32;
    let zoom = if zoom.is_finite() { zoom.max(0.0) } else { 0.0 };

    (source_px * zoom).round().max(1.0) as u32
}

fn fit_video_display_size(
    source_width: c_int,
    source_height: c_int,
    zoom: f32,
    layout: VideoTerminalLayout,
) -> VideoDisplaySize {
    let desired_w_px = target_pixels_for_source_dimension(source_width, zoom);
    let desired_h_px = target_pixels_for_source_dimension(source_height, zoom);
    let max_cols = u32::from(layout.cols.saturating_sub(FIT_MARGIN_COLS).max(1));
    let max_rows = u32::from(
        layout
            .video_area_rows
            .saturating_sub(FIT_MARGIN_ROWS)
            .max(1),
    );
    let max_w_px = max_cols * layout.cell_width_px;
    let max_h_px = max_rows * layout.cell_height_px;
    let scale = (max_w_px as f32 / desired_w_px as f32)
        .min(max_h_px as f32 / desired_h_px as f32)
        .min(1.0);
    let target_w_px = (desired_w_px as f32 * scale)
        .round()
        .max(1.0)
        .min(max_w_px as f32) as u32;
    let target_h_px = (desired_h_px as f32 * scale)
        .round()
        .max(1.0)
        .min(max_h_px as f32) as u32;

    VideoDisplaySize {
        target_cols: cells_for_pixels(target_w_px, layout.cell_width_px).min(max_cols),
        target_rows: cells_for_pixels(target_h_px, layout.cell_height_px).min(max_rows),
        target_w_px,
        target_h_px,
    }
}

pub fn play(config: &Config, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let running = playback_control::install_ctrlc_handler()?;

    unsafe {
        av_log_set_level(AV_LOG_QUIET);
        let mut format_ctx: *mut AVFormatContext = std::ptr::null_mut();
        let c_file_path = CString::new(file_path)?;

        let ret = avformat_open_input(
            &mut format_ctx,
            c_file_path.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if ret != 0 {
            return Err(format!("Could not open video file: {}", file_path).into());
        }

        let ret = avformat_find_stream_info(format_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut format_ctx);
            return Err("Could not find stream information".into());
        }

        // Find the first video and audio streams
        let mut video_stream_index = -1;
        let mut audio_stream_index = -1;
        let mut video_codecpar_ptr: *mut AVCodecParameters = std::ptr::null_mut();
        let mut audio_codecpar_ptr: *mut AVCodecParameters = std::ptr::null_mut();
        let mut video_stream_ptr: *mut AVStream = std::ptr::null_mut();
        let mut audio_stream_ptr: *mut AVStream = std::ptr::null_mut();

        for i in 0..(*format_ctx).nb_streams {
            let stream_ptr_ptr = (*format_ctx).streams.add(i as usize);
            if stream_ptr_ptr.is_null() {
                continue;
            }
            let stream = *stream_ptr_ptr;
            if stream.is_null() {
                continue;
            }
            let codecpar = (*stream).codecpar;
            if codecpar.is_null() {
                continue;
            }
            if (*codecpar).codec_type == AVMEDIA_TYPE_VIDEO && video_stream_index == -1 {
                video_stream_index = i as i32;
                video_codecpar_ptr = codecpar;
                video_stream_ptr = stream;
            } else if (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO && audio_stream_index == -1 {
                audio_stream_index = i as i32;
                audio_codecpar_ptr = codecpar;
                audio_stream_ptr = stream;
            }
        }

        if video_stream_index == -1 {
            avformat_close_input(&mut format_ctx);
            return Err("Could not find a video stream".into());
        }

        let video_decoder = avcodec_find_decoder((*video_codecpar_ptr).codec_id);
        if video_decoder.is_null() {
            avformat_close_input(&mut format_ctx);
            return Err("Video decoder not found".into());
        }

        let codec_ctx = avcodec_alloc_context3(video_decoder);
        if codec_ctx.is_null() {
            avformat_close_input(&mut format_ctx);
            return Err("Could not allocate video codec context".into());
        }

        let ret = avcodec_parameters_to_context(codec_ctx, video_codecpar_ptr);
        if ret < 0 {
            avcodec_free_context(&mut { codec_ctx });
            avformat_close_input(&mut format_ctx);
            return Err("Could not copy video codec parameters to context".into());
        }

        let ret = avcodec_open2(codec_ctx, video_decoder, std::ptr::null_mut());
        if ret < 0 {
            avcodec_free_context(&mut { codec_ctx });
            avformat_close_input(&mut format_ctx);
            return Err("Could not open video codec".into());
        }

        let mut audio_codec_ctx: *mut AVCodecContext = std::ptr::null_mut();
        let mut has_audio = audio_stream_index != -1;
        if has_audio {
            let audio_decoder = avcodec_find_decoder((*audio_codecpar_ptr).codec_id);
            if audio_decoder.is_null() {
                has_audio = false;
            } else {
                audio_codec_ctx = avcodec_alloc_context3(audio_decoder);
                if audio_codec_ctx.is_null() {
                    has_audio = false;
                } else {
                    let ret = avcodec_parameters_to_context(audio_codec_ctx, audio_codecpar_ptr);
                    if ret < 0 {
                        avcodec_free_context(&mut audio_codec_ctx);
                        audio_codec_ctx = std::ptr::null_mut();
                        has_audio = false;
                    } else {
                        let ret =
                            avcodec_open2(audio_codec_ctx, audio_decoder, std::ptr::null_mut());
                        if ret < 0 {
                            avcodec_free_context(&mut audio_codec_ctx);
                            audio_codec_ctx = std::ptr::null_mut();
                            has_audio = false;
                        }
                    }
                }
            }
        }

        let mut _cpal_stream: Option<cpal::Stream> = None;
        let mut target_channels = 2;
        let mut target_sample_rate = 48000;
        let audio_clock = Arc::new(AtomicU64::new(0));
        let reset_audio = Arc::new(AtomicBool::new(false));
        let volume_percent_atomic = Arc::new(AtomicU32::new(100));

        let mut swr_ctx: *mut SwrContext = std::ptr::null_mut();
        let mut audio_producer = None;

        if has_audio {
            let audio_setup_ctrlc = playback_control::hard_exit_on_ctrlc();
            let host = cpal::default_host();
            if let Some(device) = host.default_output_device() {
                if let Ok(config_supported) = device.default_output_config() {
                    let audio_config: cpal::StreamConfig = config_supported.into();
                    target_channels = audio_config.channels;
                    target_sample_rate = audio_config.sample_rate;

                    let rb = HeapRb::<f32>::new(
                        (target_sample_rate as usize * target_channels as usize * 2).max(1),
                    );
                    let (prod, mut cons) = rb.split();
                    audio_producer = Some(prod);

                    let audio_clock_cb = audio_clock.clone();
                    let reset_audio_cb = reset_audio.clone();
                    let volume_percent_cb = volume_percent_atomic.clone();

                    let stream_res = device.build_output_stream(
                        &audio_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            if reset_audio_cb.load(Ordering::SeqCst) {
                                while cons.try_pop().is_some() {}
                                reset_audio_cb.store(false, Ordering::SeqCst);
                            }
                            let volume = volume_percent_cb.load(Ordering::SeqCst) as f32 / 100.0;
                            let mut read = 0;
                            for sample in data.iter_mut() {
                                if let Some(val) = cons.try_pop() {
                                    *sample = val * volume;
                                    read += 1;
                                } else {
                                    *sample = 0.0;
                                }
                            }
                            audio_clock_cb.fetch_add(read as u64, Ordering::SeqCst);
                        },
                        move |err| {
                            eprintln!("CPAL Audio stream error: {}", err);
                        },
                        None,
                    );

                    if let Ok(stream) = stream_res {
                        if stream.play().is_ok() {
                            _cpal_stream = Some(stream);
                        } else {
                            has_audio = false;
                        }
                    } else {
                        has_audio = false;
                    }
                } else {
                    has_audio = false;
                }
            } else {
                has_audio = false;
            }
            drop(audio_setup_ctrlc);
        }

        let orig_w = (*video_codecpar_ptr).width;
        let orig_h = (*video_codecpar_ptr).height;
        let terminal_layout = VideoTerminalLayout::current();
        let display_size = fit_video_display_size(orig_w, orig_h, config.zoom, terminal_layout);
        let duration_ms = stream_duration_ms(video_stream_ptr);

        let fps_rational = (*video_stream_ptr).avg_frame_rate;
        let delay_secs = if fps_rational.num > 0 && fps_rational.den > 0 {
            fps_rational.den as f32 / fps_rational.num as f32
        } else {
            1.0 / 30.0
        };
        let frame_delay = config
            .fps
            .map(|f| Duration::from_secs_f32(1.0 / f))
            .unwrap_or_else(|| Duration::from_secs_f32(delay_secs));

        let _terminal_session = match VideoTerminalSession::enter() {
            Ok(session) => session,
            Err(err) => {
                if !audio_codec_ctx.is_null() {
                    avcodec_free_context(&mut audio_codec_ctx);
                }
                avcodec_free_context(&mut { codec_ctx });
                avformat_close_input(&mut format_ctx);
                return Err(err.into());
            }
        };

        let status = Arc::new(Mutex::new(StatusState {
            duration_ms,
            ..StatusState::default()
        }));
        let position_ms = Arc::new(AtomicU64::new(0));
        let playback_generation = Arc::new(AtomicU64::new(0));

        let video_channel_capacity = if has_audio { 1024 } else { 16 };
        let (render_sender, render_receiver) =
            mpsc::sync_channel::<RenderMessage>(video_channel_capacity);
        let render_thread = spawn_render_thread(
            render_receiver,
            running,
            status.clone(),
            position_ms.clone(),
            audio_clock.clone(),
            playback_generation.clone(),
            terminal_layout,
            has_audio,
            target_channels,
            target_sample_rate,
            frame_delay,
        );

        let (command_sender, command_receiver) = mpsc::channel::<PlayerCommand>();
        let input_thread = spawn_input_thread(
            running,
            command_sender,
            render_sender.clone(),
            status.clone(),
        );

        let pkt = av_packet_alloc();
        let frame = av_frame_alloc();
        let audio_frame = if has_audio {
            av_frame_alloc()
        } else {
            std::ptr::null_mut()
        };

        let mut playback = PlaybackState {
            terminal_layout,
            display_size,
            duration_ms,
            volume_percent: 100,
            generation: 0,
            drop_until_ms: None,
        };
        let mut output_buffer = output_buffer_for(display_size);
        let mut sws_ctx: *mut SwsContext = std::ptr::null_mut();
        let mut playback_error: Option<String> = None;

        'outer: loop {
            while running.load(Ordering::SeqCst) {
                match handle_pending_commands(
                    &command_receiver,
                    running,
                    &mut playback,
                    format_ctx,
                    video_stream_index,
                    video_stream_ptr,
                    codec_ctx,
                    has_audio,
                    audio_codec_ctx,
                    &reset_audio,
                    &audio_clock,
                    &playback_generation,
                    &position_ms,
                    &volume_percent_atomic,
                    &status,
                    &render_sender,
                    &mut sws_ctx,
                    &mut output_buffer,
                    orig_w,
                    orig_h,
                    config.zoom,
                ) {
                    Ok(true) => {}
                    Ok(false) => break 'outer,
                    Err(err) => {
                        playback_error = Some(err);
                        break 'outer;
                    }
                }

                let ret = av_read_frame(format_ctx, pkt);
                if ret != 0 {
                    break;
                }

                if (*pkt).stream_index == video_stream_index {
                    let ret = avcodec_send_packet(codec_ctx, pkt);
                    if ret == 0 {
                        while avcodec_receive_frame(codec_ctx, frame) == 0 {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }

                            let video_pts_ms = frame_pts_ms(frame, video_stream_ptr)
                                .unwrap_or_else(|| position_ms.load(Ordering::SeqCst));
                            if should_drop_before_target(playback.drop_until_ms, Some(video_pts_ms))
                            {
                                continue;
                            }

                            if sws_ctx.is_null() {
                                let frame_w = if (*frame).width > 0 {
                                    (*frame).width
                                } else {
                                    orig_w
                                };
                                let frame_h = if (*frame).height > 0 {
                                    (*frame).height
                                } else {
                                    orig_h
                                };
                                let frame_format = (*frame).format;

                                sws_ctx = sws_getContext(
                                    frame_w,
                                    frame_h,
                                    frame_format,
                                    playback.display_size.target_w_px as i32,
                                    playback.display_size.target_h_px as i32,
                                    AV_PIX_FMT_RGBA,
                                    SWS_BILINEAR,
                                    std::ptr::null_mut(),
                                    std::ptr::null_mut(),
                                    std::ptr::null(),
                                );

                                if sws_ctx.is_null() {
                                    running.store(false, Ordering::SeqCst);
                                    playback_error = Some(format!(
                                        "Could not initialize software scaler for decoded frame format {}",
                                        frame_format
                                    ));
                                    break;
                                }
                            }

                            let frame_h = if (*frame).height > 0 {
                                (*frame).height
                            } else {
                                orig_h
                            };
                            let dst_data: [*mut u8; 8] = [
                                output_buffer.as_mut_ptr(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            ];
                            let dst_linesize: [c_int; 8] = [
                                (playback.display_size.target_w_px * 4) as c_int,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                                0,
                            ];

                            sws_scale(
                                sws_ctx,
                                (*frame).data.as_ptr() as *const *const u8,
                                (*frame).linesize.as_ptr(),
                                0,
                                frame_h,
                                dst_data.as_ptr() as *const *mut u8,
                                dst_linesize.as_ptr(),
                            );

                            if render_sender
                                .send(RenderMessage::Frame {
                                    rgba: output_buffer.clone(),
                                    pts_ms: video_pts_ms,
                                    generation: playback.generation,
                                    display_size: playback.display_size,
                                })
                                .is_err()
                            {
                                running.store(false, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                } else if (*pkt).stream_index == audio_stream_index && has_audio {
                    let ret = avcodec_send_packet(audio_codec_ctx, pkt);
                    if ret == 0 {
                        while avcodec_receive_frame(audio_codec_ctx, audio_frame) == 0 {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }

                            let audio_pts_ms = if audio_stream_ptr.is_null() {
                                None
                            } else {
                                frame_pts_ms(audio_frame, audio_stream_ptr)
                            };
                            if should_drop_before_target(playback.drop_until_ms, audio_pts_ms) {
                                continue;
                            }

                            let frame_sample_rate = if (*audio_frame).sample_rate > 0 {
                                (*audio_frame).sample_rate
                            } else {
                                (*audio_codecpar_ptr).sample_rate
                            };
                            if frame_sample_rate <= 0 {
                                continue;
                            }

                            if swr_ctx.is_null() {
                                let frame_format = (*audio_frame).format;
                                let mut in_ch_layout = AVChannelLayout::default();
                                if (*audio_codecpar_ptr).ch_layout.nb_channels > 0 {
                                    av_channel_layout_copy(
                                        &mut in_ch_layout,
                                        &(*audio_codecpar_ptr).ch_layout,
                                    );
                                } else {
                                    av_channel_layout_default(&mut in_ch_layout, 2);
                                }

                                let mut out_ch_layout = AVChannelLayout::default();
                                av_channel_layout_default(
                                    &mut out_ch_layout,
                                    target_channels as c_int,
                                );

                                let ret = swr_alloc_set_opts2(
                                    &mut swr_ctx,
                                    &out_ch_layout,
                                    AV_SAMPLE_FMT_FLT,
                                    target_sample_rate as c_int,
                                    &in_ch_layout,
                                    frame_format,
                                    frame_sample_rate,
                                    0,
                                    std::ptr::null_mut(),
                                );

                                av_channel_layout_uninit(&mut in_ch_layout);
                                av_channel_layout_uninit(&mut out_ch_layout);

                                if ret < 0 || swr_ctx.is_null() || swr_init(swr_ctx) < 0 {
                                    if !swr_ctx.is_null() {
                                        swr_free(&mut swr_ctx);
                                    }
                                    running.store(false, Ordering::SeqCst);
                                    playback_error = Some(format!(
                                        "Could not initialize audio resampler for decoded sample format {}",
                                        frame_format
                                    ));
                                    break;
                                }
                            }

                            let max_out_samples = ((*audio_frame).nb_samples as i64
                                * target_sample_rate as i64
                                / frame_sample_rate as i64
                                + 256) as c_int;
                            let mut resampled_buffer =
                                vec![0.0f32; (max_out_samples * target_channels as c_int) as usize];

                            let mut out_ptrs: [*mut u8; 8] = [std::ptr::null_mut(); 8];
                            out_ptrs[0] = resampled_buffer.as_mut_ptr() as *mut u8;

                            let in_ptrs = if (*audio_frame).extended_data.is_null() {
                                (*audio_frame).data.as_ptr() as *const *const u8
                            } else {
                                (*audio_frame).extended_data as *const *const u8
                            };

                            let converted = swr_convert(
                                swr_ctx,
                                out_ptrs.as_mut_ptr(),
                                max_out_samples,
                                in_ptrs,
                                (*audio_frame).nb_samples,
                            );

                            if converted > 0 {
                                if let Some(ref mut prod) = audio_producer {
                                    let sample_count =
                                        (converted * target_channels as c_int) as usize;
                                    for sample in resampled_buffer.iter().take(sample_count) {
                                        while running.load(Ordering::SeqCst) {
                                            match prod.try_push(*sample) {
                                                Ok(_) => break,
                                                Err(_) => {
                                                    thread::sleep(Duration::from_micros(500));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                av_packet_unref(pkt);
                if playback_error.is_some() {
                    break;
                }
            }

            if playback_error.is_some() {
                break 'outer;
            }

            if config.loop_video && running.load(Ordering::SeqCst) {
                if let Err(err) = seek_playback_to_ms(
                    0,
                    &mut playback,
                    format_ctx,
                    video_stream_index,
                    video_stream_ptr,
                    codec_ctx,
                    has_audio,
                    audio_codec_ctx,
                    &reset_audio,
                    &audio_clock,
                    &playback_generation,
                    &position_ms,
                    &status,
                    &render_sender,
                    None,
                ) {
                    playback_error = Some(err);
                    break 'outer;
                }
            } else {
                break 'outer;
            }
        }

        running.store(false, Ordering::SeqCst);
        let _ = render_sender.send(RenderMessage::Quit);
        let _ = input_thread.join();
        drop(render_sender);
        let _ = render_thread.join();

        av_packet_free(&mut { pkt });
        av_frame_free(&mut { frame });
        if !audio_frame.is_null() {
            av_frame_free(&mut { audio_frame });
        }
        if !sws_ctx.is_null() {
            sws_freeContext(sws_ctx);
        }
        if !swr_ctx.is_null() {
            swr_free(&mut swr_ctx);
        }
        avcodec_free_context(&mut { codec_ctx });
        if !audio_codec_ctx.is_null() {
            avcodec_free_context(&mut audio_codec_ctx);
        }
        avformat_close_input(&mut format_ctx);

        if let Some(err) = playback_error {
            return Err(err.into());
        }
    }

    Ok(())
}

fn spawn_input_thread(
    running: &'static AtomicBool,
    command_sender: mpsc::Sender<PlayerCommand>,
    render_sender: SyncSender<RenderMessage>,
    status: SharedStatus,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut input_state = ControlInputState::default();

        while running.load(Ordering::SeqCst) {
            match event::poll(EVENT_POLL_INTERVAL) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key))
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        let outcome = handle_key(key, &mut input_state);
                        update_status(&status, |state| {
                            state.input_buffer = input_state.goto_buffer.clone();
                            if let Some(message) = outcome.message.clone() {
                                state.message = message;
                            }
                        });
                        if outcome.redraw_status {
                            let _ = render_sender.try_send(RenderMessage::RedrawStatus);
                        }
                        for command in outcome.commands {
                            if matches!(command, PlayerCommand::Quit) {
                                running.store(false, Ordering::SeqCst);
                                let _ = render_sender.try_send(RenderMessage::Quit);
                            }
                            let _ = command_sender.send(command);
                        }
                    }
                    Ok(Event::Resize(cols, rows)) => {
                        let geometry = TerminalGeometry::current_with_reported_cells(cols, rows);
                        let _ = command_sender.send(PlayerCommand::Resize(geometry));
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    })
}

fn handle_key(key: KeyEvent, state: &mut ControlInputState) -> KeyOutcome {
    let mut outcome = KeyOutcome::default();

    if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        outcome.commands.push(PlayerCommand::Quit);
        return outcome;
    }
    if matches!(key.code, KeyCode::Char('q')) {
        outcome.commands.push(PlayerCommand::Quit);
        return outcome;
    }

    if let Some(buffer) = &mut state.goto_buffer {
        match key.code {
            KeyCode::Esc => {
                state.goto_buffer = None;
                outcome.message = Some(String::from("Goto canceled"));
                outcome.redraw_status = true;
            }
            KeyCode::Enter => {
                if let Some(timestamp_ms) = parse_timestamp_ms(buffer) {
                    state.goto_buffer = None;
                    outcome.commands.push(PlayerCommand::SeekTo(timestamp_ms));
                    outcome.message = Some(format!("Goto {}", format_time_ms(timestamp_ms)));
                    outcome.redraw_status = true;
                } else {
                    outcome.message = Some(String::from("Invalid timestamp"));
                    outcome.redraw_status = true;
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
                outcome.redraw_status = true;
            }
            KeyCode::Char(ch)
                if (ch.is_ascii_digit() || ch == ':')
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
            {
                buffer.push(ch);
                outcome.redraw_status = true;
            }
            _ => {}
        }
        return outcome;
    }

    match key.code {
        KeyCode::Char('f') => outcome.commands.push(PlayerCommand::SeekBy(10_000)),
        KeyCode::Char('b') => outcome.commands.push(PlayerCommand::SeekBy(-10_000)),
        KeyCode::Left => outcome.commands.push(PlayerCommand::SeekBy(-5_000)),
        KeyCode::Right => outcome.commands.push(PlayerCommand::SeekBy(5_000)),
        KeyCode::Up => outcome.commands.push(PlayerCommand::VolumeBy(5)),
        KeyCode::Down => outcome.commands.push(PlayerCommand::VolumeBy(-5)),
        KeyCode::Char('g') => {
            state.goto_buffer = Some(String::new());
            outcome.message = Some(String::from("Enter timestamp"));
            outcome.redraw_status = true;
        }
        _ => {}
    }

    outcome
}

fn spawn_render_thread(
    receiver: Receiver<RenderMessage>,
    running: &'static AtomicBool,
    status: SharedStatus,
    position_ms: Arc<AtomicU64>,
    audio_clock: Arc<AtomicU64>,
    playback_generation: Arc<AtomicU64>,
    initial_layout: VideoTerminalLayout,
    has_audio: bool,
    target_channels: u16,
    target_sample_rate: u32,
    frame_delay: Duration,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        let mut layout = initial_layout;
        let mut active_generation = playback_generation.load(Ordering::SeqCst);
        let mut first_video_pts_ms: Option<u64> = None;
        let mut last_frame_time = Instant::now();
        let mut tmux_image_state = kitty::TmuxImageState::new();

        let _ = queue!(stdout, Clear(ClearType::All), MoveTo(0, 0));
        let _ = draw_status_row(&mut stdout, layout, &status);

        while running.load(Ordering::SeqCst) {
            match receiver.recv_timeout(EVENT_POLL_INTERVAL) {
                Ok(RenderMessage::Frame {
                    rgba,
                    pts_ms,
                    generation,
                    display_size,
                }) => {
                    if generation != playback_generation.load(Ordering::SeqCst) {
                        continue;
                    }
                    if generation != active_generation {
                        active_generation = generation;
                        first_video_pts_ms = None;
                        last_frame_time = Instant::now();
                    }
                    if first_video_pts_ms.is_none() {
                        first_video_pts_ms = Some(pts_ms);
                    }

                    let relative_video_ms = pts_ms.saturating_sub(first_video_pts_ms.unwrap());
                    if has_audio {
                        let played_samples = audio_clock.load(Ordering::SeqCst);
                        let audio_time_ms = played_samples as f64 * 1000.0
                            / (target_channels as f64 * target_sample_rate as f64);
                        let diff_ms = relative_video_ms as f64 - audio_time_ms;

                        if diff_ms > 10.0 {
                            sleep_interruptibly(
                                Duration::from_millis(diff_ms as u64),
                                running,
                                &playback_generation,
                                generation,
                            );
                        } else if diff_ms < -10.0 {
                            continue;
                        }
                    } else {
                        let elapsed = last_frame_time.elapsed();
                        if elapsed < frame_delay {
                            sleep_interruptibly(
                                frame_delay - elapsed,
                                running,
                                &playback_generation,
                                generation,
                            );
                        }
                        last_frame_time = Instant::now();
                    }

                    if !running.load(Ordering::SeqCst)
                        || generation != playback_generation.load(Ordering::SeqCst)
                    {
                        continue;
                    }

                    position_ms.store(pts_ms, Ordering::SeqCst);
                    update_status(&status, |state| {
                        state.current_ms = pts_ms;
                    });

                    let (origin_col, origin_row) = layout.origin_for(display_size);
                    let _ = queue!(stdout, MoveTo(origin_col, origin_row));
                    let _ = kitty::write_rgba_frame_to_with_tmux_state(
                        &mut stdout,
                        &rgba,
                        display_size.target_w_px,
                        display_size.target_h_px,
                        display_size.target_cols,
                        display_size.target_rows,
                        true,
                        origin_col,
                        Some(&mut tmux_image_state),
                    );
                    let _ = draw_status_row(&mut stdout, layout, &status);
                }
                Ok(RenderMessage::Layout(new_layout)) => {
                    layout = new_layout;
                    let _ = queue!(stdout, Clear(ClearType::All), MoveTo(0, 0));
                    let _ = draw_status_row(&mut stdout, layout, &status);
                }
                Ok(RenderMessage::Reset { generation }) => {
                    active_generation = generation;
                    first_video_pts_ms = None;
                    last_frame_time = Instant::now();
                    let _ = draw_status_row(&mut stdout, layout, &status);
                }
                Ok(RenderMessage::RedrawStatus) => {
                    let _ = draw_status_row(&mut stdout, layout, &status);
                }
                Ok(RenderMessage::Quit) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

fn sleep_interruptibly(
    duration: Duration,
    running: &AtomicBool,
    playback_generation: &AtomicU64,
    expected_generation: u64,
) {
    let mut slept = Duration::ZERO;
    let chunk = Duration::from_millis(10);
    while slept < duration
        && running.load(Ordering::SeqCst)
        && playback_generation.load(Ordering::SeqCst) == expected_generation
    {
        let remaining = duration - slept;
        let sleep_for = remaining.min(chunk);
        thread::sleep(sleep_for);
        slept += sleep_for;
    }
}

fn draw_status_row<W: Write>(
    writer: &mut W,
    layout: VideoTerminalLayout,
    status: &SharedStatus,
) -> io::Result<()> {
    if let Some(row) = layout.status_row {
        let snapshot = status
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| StatusState::default());
        let text = status_text(&snapshot);
        queue!(
            writer,
            MoveTo(0, row),
            Clear(ClearType::CurrentLine),
            Print(truncate_to_cols(&text, layout.cols))
        )?;
        writer.flush()?;
    }
    Ok(())
}

fn status_text(status: &StatusState) -> String {
    let duration = status
        .duration_ms
        .map(format_time_ms)
        .unwrap_or_else(|| String::from("--:--"));
    let middle = if let Some(input) = &status.input_buffer {
        if status.message.is_empty() {
            format!("Goto> {input}  Enter seek Esc cancel")
        } else {
            format!("Goto> {input} | {} | Enter seek Esc cancel", status.message)
        }
    } else {
        status.message.clone()
    };
    if middle.is_empty() {
        format!(
            "{} / {} | Vol {}% | {}",
            format_time_ms(status.current_ms),
            duration,
            status.volume_percent,
            STATUS_SHORTCUTS
        )
    } else {
        format!(
            "{} / {} | Vol {}% | {} | {}",
            format_time_ms(status.current_ms),
            duration,
            status.volume_percent,
            middle,
            STATUS_SHORTCUTS
        )
    }
}

fn truncate_to_cols(text: &str, cols: u16) -> String {
    text.chars().take(cols as usize).collect()
}

fn format_time_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn parse_timestamp_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let parts: Vec<&str> = value.split(':').collect();
    if parts.iter().any(|part| part.is_empty()) || parts.len() > 3 {
        return None;
    }
    let numbers: Option<Vec<u64>> = parts.iter().map(|part| part.parse::<u64>().ok()).collect();
    let numbers = numbers?;

    let seconds = match numbers.as_slice() {
        [seconds] => *seconds,
        [minutes, seconds] if *seconds < 60 => minutes * 60 + seconds,
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => {
            hours * 3600 + minutes * 60 + seconds
        }
        _ => return None,
    };

    Some(seconds * 1000)
}

unsafe fn handle_pending_commands(
    command_receiver: &Receiver<PlayerCommand>,
    running: &AtomicBool,
    playback: &mut PlaybackState,
    format_ctx: *mut AVFormatContext,
    video_stream_index: c_int,
    video_stream_ptr: *mut AVStream,
    codec_ctx: *mut AVCodecContext,
    has_audio: bool,
    audio_codec_ctx: *mut AVCodecContext,
    reset_audio: &AtomicBool,
    audio_clock: &AtomicU64,
    playback_generation: &AtomicU64,
    position_ms: &AtomicU64,
    volume_percent_atomic: &AtomicU32,
    status: &SharedStatus,
    render_sender: &SyncSender<RenderMessage>,
    sws_ctx: &mut *mut SwsContext,
    output_buffer: &mut Vec<u8>,
    source_width: c_int,
    source_height: c_int,
    zoom: f32,
) -> Result<bool, String> {
    while let Ok(command) = command_receiver.try_recv() {
        match command {
            PlayerCommand::SeekBy(delta_ms) => {
                let current_ms = position_ms.load(Ordering::SeqCst);
                let target_ms = if delta_ms.is_negative() {
                    current_ms.saturating_sub(delta_ms.unsigned_abs())
                } else {
                    current_ms.saturating_add(delta_ms as u64)
                };
                let direction = if delta_ms >= 0 { "+" } else { "-" };
                let message = format!("Seek {direction}{}s", delta_ms.unsigned_abs() / 1000);
                seek_playback_to_ms(
                    target_ms,
                    playback,
                    format_ctx,
                    video_stream_index,
                    video_stream_ptr,
                    codec_ctx,
                    has_audio,
                    audio_codec_ctx,
                    reset_audio,
                    audio_clock,
                    playback_generation,
                    position_ms,
                    status,
                    render_sender,
                    Some(message),
                )?;
            }
            PlayerCommand::SeekTo(target_ms) => {
                seek_playback_to_ms(
                    target_ms,
                    playback,
                    format_ctx,
                    video_stream_index,
                    video_stream_ptr,
                    codec_ctx,
                    has_audio,
                    audio_codec_ctx,
                    reset_audio,
                    audio_clock,
                    playback_generation,
                    position_ms,
                    status,
                    render_sender,
                    Some(format!("Goto {}", format_time_ms(target_ms))),
                )?;
            }
            PlayerCommand::VolumeBy(delta) => {
                let next = adjust_volume_percent(playback.volume_percent, delta);
                playback.volume_percent = next;
                volume_percent_atomic.store(next, Ordering::SeqCst);
                update_status(status, |state| {
                    state.volume_percent = next;
                    state.message = format!("Volume {next}%");
                });
                let _ = render_sender.try_send(RenderMessage::RedrawStatus);
            }
            PlayerCommand::Resize(geometry) => {
                let layout = VideoTerminalLayout::from_geometry(geometry);
                let display_size =
                    fit_video_display_size(source_width, source_height, zoom, layout);
                playback.terminal_layout = layout;
                if display_size != playback.display_size {
                    playback.display_size = display_size;
                    *output_buffer = output_buffer_for(display_size);
                    if !(*sws_ctx).is_null() {
                        sws_freeContext(*sws_ctx);
                        *sws_ctx = std::ptr::null_mut();
                    }
                }
                update_status(status, |state| {
                    state.message = String::from("Resized");
                });
                let _ = render_sender.send(RenderMessage::Layout(layout));
            }
            PlayerCommand::Quit => {
                running.store(false, Ordering::SeqCst);
                let _ = render_sender.try_send(RenderMessage::Quit);
                return Ok(false);
            }
        }
    }

    Ok(true)
}

unsafe fn seek_playback_to_ms(
    target_ms: u64,
    playback: &mut PlaybackState,
    format_ctx: *mut AVFormatContext,
    video_stream_index: c_int,
    video_stream_ptr: *mut AVStream,
    codec_ctx: *mut AVCodecContext,
    has_audio: bool,
    audio_codec_ctx: *mut AVCodecContext,
    reset_audio: &AtomicBool,
    audio_clock: &AtomicU64,
    playback_generation: &AtomicU64,
    position_ms: &AtomicU64,
    status: &SharedStatus,
    render_sender: &SyncSender<RenderMessage>,
    message: Option<String>,
) -> Result<u64, String> {
    let target_ms = clamp_seek_ms(target_ms, playback.duration_ms);
    let timestamp = ms_to_stream_timestamp(target_ms, video_stream_ptr);
    let ret = av_seek_frame(
        format_ctx,
        video_stream_index,
        timestamp,
        AVSEEK_FLAG_BACKWARD,
    );
    if ret < 0 {
        update_status(status, |state| {
            state.message = String::from("Seek failed");
        });
        let _ = render_sender.try_send(RenderMessage::RedrawStatus);
        return Err(String::from("Seek failed"));
    }

    avcodec_flush_buffers(codec_ctx);
    if has_audio && !audio_codec_ctx.is_null() {
        avcodec_flush_buffers(audio_codec_ctx);
        reset_audio.store(true, Ordering::SeqCst);
        audio_clock.store(0, Ordering::SeqCst);
    }

    playback.generation = playback.generation.saturating_add(1);
    playback.drop_until_ms = Some(target_ms);
    playback_generation.store(playback.generation, Ordering::SeqCst);
    position_ms.store(target_ms, Ordering::SeqCst);
    update_status(status, |state| {
        state.current_ms = target_ms;
        state.input_buffer = None;
        if let Some(message) = message {
            state.message = message;
        }
    });
    let _ = render_sender.send(RenderMessage::Reset {
        generation: playback.generation,
    });
    let _ = render_sender.try_send(RenderMessage::RedrawStatus);

    Ok(target_ms)
}

fn clamp_seek_ms(target_ms: u64, duration_ms: Option<u64>) -> u64 {
    duration_ms.map_or(target_ms, |duration| target_ms.min(duration))
}

fn adjust_volume_percent(current: u32, delta: i32) -> u32 {
    (current as i32 + delta).clamp(0, 200) as u32
}

fn output_buffer_for(display_size: VideoDisplaySize) -> Vec<u8> {
    vec![0u8; (display_size.target_w_px * display_size.target_h_px * 4) as usize]
}

fn should_drop_before_target(drop_until_ms: Option<u64>, pts_ms: Option<u64>) -> bool {
    match (drop_until_ms, pts_ms) {
        (Some(target_ms), Some(pts_ms)) => pts_ms < target_ms,
        _ => false,
    }
}

unsafe fn stream_duration_ms(stream: *mut AVStream) -> Option<u64> {
    if stream.is_null() {
        return None;
    }
    let duration = (*stream).duration;
    let time_base = (*stream).time_base;
    if duration <= 0 || time_base.num <= 0 || time_base.den <= 0 {
        return None;
    }
    Some((duration as f64 * time_base.num as f64 * 1000.0 / time_base.den as f64).round() as u64)
}

unsafe fn frame_pts_ms(frame: *mut AVFrame, stream: *mut AVStream) -> Option<u64> {
    if frame.is_null() || stream.is_null() {
        return None;
    }
    let pts = (*frame).pts;
    if pts == AV_NOPTS_VALUE || pts < 0 {
        return None;
    }
    let time_base = (*stream).time_base;
    if time_base.num <= 0 || time_base.den <= 0 {
        return None;
    }
    Some((pts as f64 * time_base.num as f64 * 1000.0 / time_base.den as f64).round() as u64)
}

unsafe fn ms_to_stream_timestamp(ms: u64, stream: *mut AVStream) -> i64 {
    if stream.is_null() {
        return 0;
    }
    let time_base = (*stream).time_base;
    if time_base.num <= 0 || time_base.den <= 0 {
        return 0;
    }
    (ms as f64 * time_base.den as f64 / (1000.0 * time_base.num as f64)).round() as i64
}

fn update_status(status: &SharedStatus, update: impl FnOnce(&mut StatusState)) {
    if let Ok(mut state) = status.lock() {
        update(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_from_cells(cols: u16, rows: u16) -> VideoTerminalLayout {
        VideoTerminalLayout::from_geometry(TerminalGeometry::from_cells(cols, rows))
    }

    #[test]
    fn default_video_zoom_uses_original_source_size() {
        let layout =
            VideoTerminalLayout::from_geometry(TerminalGeometry::with_cell_size(200, 80, 10, 20));
        let size = fit_video_display_size(1280, 720, 1.0, layout);

        assert_eq!(size.target_w_px, 1280);
        assert_eq!(size.target_h_px, 720);
        assert_eq!(size.target_cols, 128);
        assert_eq!(size.target_rows, 36);
    }

    #[test]
    fn video_zoom_is_relative_to_original_source_size() {
        let layout =
            VideoTerminalLayout::from_geometry(TerminalGeometry::with_cell_size(240, 80, 10, 20));
        let size = fit_video_display_size(1280, 720, 1.5, layout);

        assert_eq!(size.target_w_px, 1920);
        assert_eq!(size.target_h_px, 1080);
        assert_eq!(size.target_cols, 192);
        assert_eq!(size.target_rows, 54);
    }

    #[test]
    fn video_layout_centers_when_space_allows() {
        let layout = layout_from_cells(100, 30);
        let size = fit_video_display_size(1280, 720, 1.0, layout);

        assert_eq!(size.target_cols, 96);
        assert_eq!(size.target_rows, 27);
        assert_eq!(layout.origin_for(size), (2, 1));
        assert_eq!(layout.status_row, Some(29));
    }

    #[test]
    fn video_layout_clamps_to_available_video_area() {
        let layout = layout_from_cells(40, 10);
        let size = fit_video_display_size(1280, 720, 1.0, layout);

        assert_eq!(size.target_cols, 25);
        assert_eq!(size.target_rows, 7);
        assert_eq!(layout.origin_for(size), (7, 1));
    }

    #[test]
    fn video_layout_handles_tiny_terminals() {
        let layout = layout_from_cells(10, 1);
        let size = fit_video_display_size(1280, 720, 1.0, layout);

        assert_eq!(layout.status_row, None);
        assert_eq!(layout.video_area_rows, 1);
        assert_eq!(size.target_rows, 1);
        assert_eq!(size.target_cols, 4);
        assert_eq!(layout.origin_for(size).1, 0);
    }

    #[test]
    fn video_uses_reported_cell_geometry_for_occupied_cells() {
        let layout =
            VideoTerminalLayout::from_geometry(TerminalGeometry::with_cell_size(120, 40, 12, 20));
        let size = fit_video_display_size(640, 360, 1.0, layout);

        assert_eq!(size.target_w_px, 640);
        assert_eq!(size.target_h_px, 360);
        assert_eq!(size.target_cols, 54);
        assert_eq!(size.target_rows, 18);
    }

    #[test]
    fn status_row_truncates_to_terminal_width() {
        assert_eq!(truncate_to_cols("abcdef", 3), "abc");
    }

    #[test]
    fn timestamp_parser_accepts_flexible_formats() {
        assert_eq!(parse_timestamp_ms("90"), Some(90_000));
        assert_eq!(parse_timestamp_ms("1:30"), Some(90_000));
        assert_eq!(parse_timestamp_ms("1:02:03"), Some(3_723_000));
    }

    #[test]
    fn timestamp_parser_rejects_invalid_formats() {
        assert_eq!(parse_timestamp_ms(""), None);
        assert_eq!(parse_timestamp_ms("1:"), None);
        assert_eq!(parse_timestamp_ms("1:75"), None);
        assert_eq!(parse_timestamp_ms("1:2:99"), None);
        assert_eq!(parse_timestamp_ms("a:b"), None);
    }

    #[test]
    fn keyboard_shortcuts_map_to_player_commands() {
        let mut state = ControlInputState::default();

        assert_eq!(
            handle_key(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
                &mut state
            )
            .commands,
            vec![PlayerCommand::SeekBy(10_000)]
        );
        assert_eq!(
            handle_key(
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
                &mut state
            )
            .commands,
            vec![PlayerCommand::SeekBy(-10_000)]
        );
        assert_eq!(
            handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &mut state).commands,
            vec![PlayerCommand::SeekBy(-5_000)]
        );
        assert_eq!(
            handle_key(
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
                &mut state
            )
            .commands,
            vec![PlayerCommand::SeekBy(5_000)]
        );
        assert_eq!(
            handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut state).commands,
            vec![PlayerCommand::VolumeBy(5)]
        );
        assert_eq!(
            handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut state).commands,
            vec![PlayerCommand::VolumeBy(-5)]
        );
        assert_eq!(
            handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut state
            )
            .commands,
            vec![PlayerCommand::Quit]
        );
    }

    #[test]
    fn goto_input_accepts_entered_timestamp() {
        let mut state = ControlInputState::default();
        handle_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            &mut state,
        );
        for ch in "1:30".chars() {
            let modifiers = if ch == ':' {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            handle_key(KeyEvent::new(KeyCode::Char(ch), modifiers), &mut state);
        }

        let outcome = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );

        assert_eq!(outcome.commands, vec![PlayerCommand::SeekTo(90_000)]);
        assert_eq!(state.goto_buffer, None);
    }

    #[test]
    fn volume_clamps_to_supported_range() {
        assert_eq!(adjust_volume_percent(100, 5), 105);
        assert_eq!(adjust_volume_percent(100, -5), 95);
        assert_eq!(adjust_volume_percent(195, 10), 200);
        assert_eq!(adjust_volume_percent(5, -10), 0);
    }
}
