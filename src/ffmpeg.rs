use std::ffi::{c_char, c_double, c_int, c_uint, c_void};

pub const AVMEDIA_TYPE_VIDEO: c_int = 0;
pub const AVMEDIA_TYPE_AUDIO: c_int = 1;
pub const AV_PIX_FMT_RGBA: c_int = 26;
pub const SWS_BILINEAR: c_int = 2;
pub const AV_SAMPLE_FMT_FLT: c_int = 3;
pub const AV_LOG_QUIET: c_int = -8;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AVChannelLayout {
    pub order: c_int,
    pub nb_channels: c_int,
    pub mask: u64,
    pub opaque: *mut c_void,
}

impl Default for AVChannelLayout {
    fn default() -> Self {
        Self {
            order: 0,
            nb_channels: 0,
            mask: 0,
            opaque: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

// FFmpeg 7.x+ AVCodecParameters: coded_side_data was moved up next to extradata_size
// and the deprecated channel_layout/channels fields were removed.
#[cfg(not(ffmpeg_old_channel_layout))]
#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type: c_int,
    pub codec_id: u32,
    pub codec_tag: u32,
    pub _pad1: u32, // Padding for 8-byte pointer alignment
    pub extradata: *mut u8,
    pub extradata_size: c_int,
    pub _pad2: u32, // Padding for 8-byte pointer alignment
    pub coded_side_data: *mut c_void,
    pub nb_coded_side_data: c_int,
    pub format: c_int,
    pub bit_rate: i64,
    pub bits_per_coded_sample: c_int,
    pub bits_per_raw_sample: c_int,
    pub profile: c_int,
    pub level: c_int,
    pub width: c_int,
    pub height: c_int,
    pub _pad3: [u8; 48],
    pub ch_layout: AVChannelLayout,
    pub sample_rate: c_int,
}

// FFmpeg 6.x AVCodecParameters: coded_side_data lives at the end of the struct and the
// deprecated channel_layout/channels are still present (FF_API_OLD_CHANNEL_LAYOUT).
#[cfg(ffmpeg_old_channel_layout)]
#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type: c_int,
    pub codec_id: u32,
    pub codec_tag: u32,
    // implicit 4-byte pad here for 8-byte ptr alignment
    pub extradata: *mut u8,
    pub extradata_size: c_int,
    pub format: c_int,
    pub bit_rate: i64,
    pub bits_per_coded_sample: c_int,
    pub bits_per_raw_sample: c_int,
    pub profile: c_int,
    pub level: c_int,
    pub width: c_int,
    pub height: c_int,
    pub sample_aspect_ratio: AVRational,
    pub field_order: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub color_space: c_int,
    pub chroma_location: c_int,
    pub video_delay: c_int,
    // implicit 4-byte pad here for 8-byte u64 alignment
    pub channel_layout: u64, // deprecated, FF_API_OLD_CHANNEL_LAYOUT
    pub channels: c_int,     // deprecated, FF_API_OLD_CHANNEL_LAYOUT
    pub sample_rate: c_int,
    pub block_align: c_int,
    pub frame_size: c_int,
    pub initial_padding: c_int,
    pub trailing_padding: c_int,
    pub seek_preroll: c_int,
    // implicit 4-byte pad here for 8-byte AVChannelLayout alignment
    pub ch_layout: AVChannelLayout,
    pub framerate: AVRational,
    pub coded_side_data: *mut c_void,
    pub nb_coded_side_data: c_int,
}

#[repr(C)]
pub struct AVStream {
    pub av_class: *const c_void,
    pub index: c_int,
    pub id: c_int,
    pub codecpar: *mut AVCodecParameters,
    pub priv_data: *mut c_void,
    pub time_base: AVRational,
    pub start_time: i64,
    pub duration: i64,
    pub nb_frames: i64,
    pub disposition: c_int,
    pub discard: c_int,
    pub sample_aspect_ratio: AVRational,
    pub metadata: *mut c_void,
    pub avg_frame_rate: AVRational,
}

#[repr(C)]
pub struct AVFormatContext {
    pub av_class: *const c_void,
    pub iformat: *mut c_void,
    pub oformat: *mut c_void,
    pub priv_data: *mut c_void,
    pub pb: *mut c_void,
    pub ctx_flags: c_int,
    pub nb_streams: c_uint,
    pub streams: *mut *mut AVStream,
}

#[repr(C)]
pub struct AVCodecContext {
    pub av_class: *const c_void,
    pub log_level_offset: c_int,
    pub codec_type: c_int,
    pub codec: *const c_void,
    pub name: [c_char; 32],
    pub codec_id: u32,
    pub codec_tag: u32,
    pub priv_data: *mut c_void,
    pub internal: *mut c_void,
    pub opaque: *mut c_void,
    pub bit_rate: i64,
    pub bit_rate_tolerance: c_int,
    pub global_quality: c_int,
    pub compression_level: c_int,
    pub flags: c_int,
    pub flags2: c_int,
    pub extradata: *mut u8,
    pub extradata_size: c_int,
    pub time_base: AVRational,
    // Note: Other fields exist in AVCodecContext, but since we only allocate
    // AVCodecContext on the heap via avcodec_alloc_context3, the Rust compiler
    // doesn't need the exact struct size, just the pointer.
}

// FFmpeg 7.x+ AVFrame: many deprecated fields (key_frame, coded_picture_number,
// interlaced_frame, palette_has_changed, reordered_opaque, channel_layout) were removed.
#[cfg(not(ffmpeg_old_channel_layout))]
#[repr(C)]
pub struct AVFrame {
    pub data: [*mut u8; 8],
    pub linesize: [c_int; 8],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
    pub _pad_pts: [u8; 16],
    pub pts: i64,
    pub _pad1: [u8; 36],
    pub sample_rate: c_int,
    pub _pad2: [u8; 200],
    pub ch_layout: AVChannelLayout,
}

// FFmpeg 6.x AVFrame: extra deprecated fields between pts and sample_rate push
// sample_rate further back by 28 bytes (key_frame/pict_struct/etc. between format
// and pts, and coded_picture_number/display_picture_number/interlaced_frame/
// top_field_first/palette_has_changed/reordered_opaque after time_base).
#[cfg(ffmpeg_old_channel_layout)]
#[repr(C)]
pub struct AVFrame {
    pub data: [*mut u8; 8],
    pub linesize: [c_int; 8],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
    // key_frame (4) + pict_type (4) + sample_aspect_ratio (8) = 16 bytes before pts
    pub _pad_pts: [u8; 16],
    pub pts: i64,
    // pkt_dts (8) + time_base (8) + coded_picture_number (4) + display_picture_number (4)
    // + quality (4) + pad (4) + opaque (8) + repeat_pict (4) + interlaced_frame (4)
    // + top_field_first (4) + palette_has_changed (4) + pad (4) + reordered_opaque (8)
    // = 68 bytes between pts and sample_rate, but pts ends at +8, so padding is 64.
    pub _pad1: [u8; 64],
    pub sample_rate: c_int,
}

#[repr(C)]
pub struct AVPacket {
    pub buf: *mut c_void,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    pub stream_index: c_int,
}

pub enum SwsContext {}
pub enum SwrContext {}

extern "C" {
    pub fn av_log_set_level(level: c_int);
    pub fn av_channel_layout_default(ch_layout: *mut AVChannelLayout, nb_channels: c_int);
    pub fn av_channel_layout_copy(dst: *mut AVChannelLayout, src: *const AVChannelLayout) -> c_int;
    pub fn av_channel_layout_uninit(ch_layout: *mut AVChannelLayout);

    pub fn swr_alloc_set_opts2(
        ps: *mut *mut SwrContext,
        out_ch_layout: *const AVChannelLayout,
        out_sample_fmt: c_int,
        out_sample_rate: c_int,
        in_ch_layout: *const AVChannelLayout,
        in_sample_fmt: c_int,
        in_sample_rate: c_int,
        log_offset: c_int,
        log_ctx: *mut c_void,
    ) -> c_int;

    pub fn swr_init(s: *mut SwrContext) -> c_int;
    pub fn swr_free(s: *mut *mut SwrContext);

    pub fn swr_convert(
        s: *mut SwrContext,
        out: *mut *mut u8,
        out_count: c_int,
        in_: *const *const u8,
        in_count: c_int,
    ) -> c_int;
    // Format
    pub fn avformat_open_input(
        ps: *mut *mut AVFormatContext,
        url: *const c_char,
        fmt: *mut c_void,
        options: *mut *mut c_void,
    ) -> c_int;

    pub fn avformat_find_stream_info(ic: *mut AVFormatContext, options: *mut *mut c_void) -> c_int;

    pub fn avformat_close_input(ps: *mut *mut AVFormatContext);

    // Codec
    pub fn avcodec_find_decoder(id: u32) -> *mut c_void;
    pub fn avcodec_alloc_context3(codec: *const c_void) -> *mut AVCodecContext;
    pub fn avcodec_parameters_to_context(
        codec: *mut AVCodecContext,
        par: *const AVCodecParameters,
    ) -> c_int;
    pub fn avcodec_open2(
        avctx: *mut AVCodecContext,
        codec: *const c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    pub fn avcodec_free_context(avctx: *mut *mut AVCodecContext);

    // Packet
    pub fn av_packet_alloc() -> *mut AVPacket;
    pub fn av_packet_free(pkt: *mut *mut AVPacket);
    pub fn av_packet_unref(pkt: *mut AVPacket);

    // Frame
    pub fn av_frame_alloc() -> *mut AVFrame;
    pub fn av_frame_free(frame: *mut *mut AVFrame);

    // Decoding
    pub fn av_read_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    pub fn avcodec_send_packet(avctx: *mut AVCodecContext, avpkt: *const AVPacket) -> c_int;
    pub fn avcodec_receive_frame(avctx: *mut AVCodecContext, frame: *mut AVFrame) -> c_int;
    pub fn avcodec_flush_buffers(avctx: *mut AVCodecContext);

    // Seeking
    pub fn av_seek_frame(
        s: *mut AVFormatContext,
        stream_index: c_int,
        timestamp: i64,
        flags: c_int,
    ) -> c_int;

    // Scaling
    pub fn sws_getContext(
        srcW: c_int,
        srcH: c_int,
        srcFormat: c_int,
        dstW: c_int,
        dstH: c_int,
        dstFormat: c_int,
        flags: c_int,
        srcFilter: *mut c_void,
        dstFilter: *mut c_void,
        param: *const c_double,
    ) -> *mut SwsContext;

    pub fn sws_freeContext(swsContext: *mut SwsContext);

    pub fn sws_scale(
        c: *mut SwsContext,
        srcSlice: *const *const u8,
        srcStride: *const c_int,
        srcSliceY: c_int,
        srcSliceH: c_int,
        dst: *const *mut u8,
        dstStride: *const c_int,
    ) -> c_int;
}
