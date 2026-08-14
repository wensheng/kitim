use crate::config::Config;
use crate::ffmpeg::*;
use crate::playback_control;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::ffi::{c_int, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
            return Err(format!("Could not open audio file: {}", file_path).into());
        }

        let ret = avformat_find_stream_info(format_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut format_ctx);
            return Err("Could not find stream information".into());
        }

        let mut audio_stream_index = -1;
        let mut audio_codecpar_ptr: *mut AVCodecParameters = std::ptr::null_mut();

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
            if (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO {
                audio_stream_index = i as i32;
                audio_codecpar_ptr = codecpar;
                break;
            }
        }

        if audio_stream_index == -1 {
            avformat_close_input(&mut format_ctx);
            return Err("Could not find an audio stream".into());
        }

        let audio_decoder = avcodec_find_decoder((*audio_codecpar_ptr).codec_id);
        if audio_decoder.is_null() {
            avformat_close_input(&mut format_ctx);
            return Err("Audio decoder not found".into());
        }

        let mut audio_codec_ctx = avcodec_alloc_context3(audio_decoder);
        if audio_codec_ctx.is_null() {
            avformat_close_input(&mut format_ctx);
            return Err("Could not allocate audio codec context".into());
        }

        let ret = avcodec_parameters_to_context(audio_codec_ctx, audio_codecpar_ptr);
        if ret < 0 {
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err("Could not copy audio codec parameters to context".into());
        }

        let ret = avcodec_open2(audio_codec_ctx, audio_decoder, std::ptr::null_mut());
        if ret < 0 {
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err("Could not open audio codec".into());
        }

        let audio_setup_ctrlc = playback_control::hard_exit_on_ctrlc();
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(device) => device,
            None => {
                avcodec_free_context(&mut audio_codec_ctx);
                avformat_close_input(&mut format_ctx);
                return Err("No default audio output device".into());
            }
        };
        let config_supported = match device.default_output_config() {
            Ok(config_supported) => config_supported,
            Err(err) => {
                avcodec_free_context(&mut audio_codec_ctx);
                avformat_close_input(&mut format_ctx);
                return Err(format!("Could not get default audio output config: {}", err).into());
            }
        };
        let audio_config: cpal::StreamConfig = config_supported.into();
        let target_channels = audio_config.channels;
        let target_sample_rate = audio_config.sample_rate;

        let rb =
            HeapRb::<f32>::new((target_sample_rate as usize * target_channels as usize * 2).max(1));
        let (mut audio_producer, mut cons) = rb.split();
        let audio_clock = Arc::new(AtomicU64::new(0));
        let audio_clock_cb = audio_clock.clone();

        let stream_res = device.build_output_stream(
            &audio_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut read = 0;
                for sample in data.iter_mut() {
                    if let Some(val) = cons.try_pop() {
                        *sample = val;
                        read += 1;
                    } else {
                        *sample = 0.0;
                    }
                }
                audio_clock_cb.fetch_add(read as u64, Ordering::SeqCst);
            },
            move |err| {
                eprintln!("CPAL audio stream error: {}", err);
            },
            None,
        );

        let cpal_stream = match stream_res {
            Ok(stream) => stream,
            Err(err) => {
                avcodec_free_context(&mut audio_codec_ctx);
                avformat_close_input(&mut format_ctx);
                return Err(format!("Could not build audio output stream: {}", err).into());
            }
        };
        if let Err(err) = cpal_stream.play() {
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err(format!("Could not start audio output stream: {}", err).into());
        }
        drop(audio_setup_ctrlc);

        let input_sample_rate = (*audio_codecpar_ptr).sample_rate;
        if input_sample_rate <= 0 {
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err("Audio stream has no sample rate".into());
        }

        let mut swr_ctx: *mut SwrContext = std::ptr::null_mut();

        let pkt = av_packet_alloc();
        if pkt.is_null() {
            swr_free(&mut swr_ctx);
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err("Could not allocate audio packet".into());
        }

        let audio_frame = av_frame_alloc();
        if audio_frame.is_null() {
            av_packet_free(&mut { pkt });
            swr_free(&mut swr_ctx);
            avcodec_free_context(&mut audio_codec_ctx);
            avformat_close_input(&mut format_ctx);
            return Err("Could not allocate audio frame".into());
        }

        eprintln!("Playing audio: {}", file_path);

        let mut queued_samples = 0u64;

        'outer: loop {
            while running.load(Ordering::SeqCst) {
                let ret = av_read_frame(format_ctx, pkt);
                if ret != 0 {
                    break;
                }

                if (*pkt).stream_index == audio_stream_index {
                    let ret = avcodec_send_packet(audio_codec_ctx, pkt);
                    if ret == 0 {
                        queued_samples += receive_and_queue_audio(
                            audio_codec_ctx,
                            audio_frame,
                            &mut swr_ctx,
                            audio_codecpar_ptr,
                            input_sample_rate,
                            target_sample_rate,
                            target_channels,
                            &mut audio_producer,
                            running,
                        )?;
                    }
                }

                av_packet_unref(pkt);
            }

            if running.load(Ordering::SeqCst) {
                let _ = avcodec_send_packet(audio_codec_ctx, std::ptr::null());
                queued_samples += receive_and_queue_audio(
                    audio_codec_ctx,
                    audio_frame,
                    &mut swr_ctx,
                    audio_codecpar_ptr,
                    input_sample_rate,
                    target_sample_rate,
                    target_channels,
                    &mut audio_producer,
                    running,
                )?;
            }

            if config.loop_video && running.load(Ordering::SeqCst) {
                av_seek_frame(format_ctx, audio_stream_index, 0, 4);
                avcodec_flush_buffers(audio_codec_ctx);
            } else {
                break 'outer;
            }
        }

        while running.load(Ordering::SeqCst) && audio_clock.load(Ordering::SeqCst) < queued_samples
        {
            std::thread::sleep(Duration::from_millis(10));
        }

        av_frame_free(&mut { audio_frame });
        av_packet_free(&mut { pkt });
        swr_free(&mut swr_ctx);
        avcodec_free_context(&mut audio_codec_ctx);
        avformat_close_input(&mut format_ctx);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn receive_and_queue_audio<P: Producer<Item = f32>>(
    audio_codec_ctx: *mut AVCodecContext,
    audio_frame: *mut AVFrame,
    swr_ctx: &mut *mut SwrContext,
    audio_codecpar_ptr: *mut AVCodecParameters,
    input_sample_rate: c_int,
    target_sample_rate: u32,
    target_channels: u16,
    audio_producer: &mut P,
    running: &AtomicBool,
) -> Result<u64, String> {
    let mut queued_samples = 0u64;

    while avcodec_receive_frame(audio_codec_ctx, audio_frame) == 0 {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let frame_sample_rate = if (*audio_frame).sample_rate > 0 {
            (*audio_frame).sample_rate
        } else {
            input_sample_rate
        };
        if frame_sample_rate <= 0 {
            continue;
        }

        if (*swr_ctx).is_null() {
            let frame_format = (*audio_frame).format;
            let mut in_ch_layout = AVChannelLayout::default();
            if (*audio_codecpar_ptr).ch_layout.nb_channels > 0 {
                av_channel_layout_copy(&mut in_ch_layout, &(*audio_codecpar_ptr).ch_layout);
            } else {
                av_channel_layout_default(&mut in_ch_layout, 2);
            }

            let mut out_ch_layout = AVChannelLayout::default();
            av_channel_layout_default(&mut out_ch_layout, target_channels as c_int);

            let ret = swr_alloc_set_opts2(
                swr_ctx,
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

            if ret < 0 || (*swr_ctx).is_null() || swr_init(*swr_ctx) < 0 {
                if !(*swr_ctx).is_null() {
                    swr_free(swr_ctx);
                }
                return Err(format!(
                    "Could not initialize audio resampler for decoded sample format {}",
                    frame_format
                ));
            }
        }

        let max_out_samples = ((*audio_frame).nb_samples as i64 * target_sample_rate as i64
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
            *swr_ctx,
            out_ptrs.as_mut_ptr(),
            max_out_samples,
            in_ptrs,
            (*audio_frame).nb_samples,
        );

        if converted <= 0 {
            continue;
        }

        let sample_count = (converted * target_channels as c_int) as usize;
        for sample in resampled_buffer.iter().take(sample_count) {
            while running.load(Ordering::SeqCst) {
                match audio_producer.try_push(*sample) {
                    Ok(_) => {
                        queued_samples += 1;
                        break;
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                }
            }
        }
    }

    Ok(queued_samples)
}
