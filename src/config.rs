use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "viewim",
    version,
    about = "Kitty graphics-based terminal image, audio, and video player"
)]
pub struct Config {
    /// The image, audio, or video files to display/play
    #[arg(required_unless_present = "preview", conflicts_with = "preview")]
    pub files: Vec<String>,

    /// Zoom multiplier applied to the original media size
    #[arg(short = 'z', long = "zoom", default_value_t = 1.0)]
    pub zoom: f32,

    /// Optional frame rate override for video playback (fps)
    #[arg(short = 'f', long = "frame-rate")]
    pub fps: Option<f32>,

    /// Loop video, GIF, or audio playback continuously
    #[arg(short = 'l', long = "loop")]
    pub loop_video: bool,

    /// Render FILE into a COLS x ROWS cell area for another program's preview
    /// pane: kitty graphics on stdout, no other terminal I/O
    #[arg(long, num_args = 3, value_names = ["FILE", "COLS", "ROWS"])]
    pub preview: Option<Vec<String>>,

    /// Cell size in pixels for --preview, e.g. 9x18 [default: from the tty]
    #[arg(long, value_name = "WxH")]
    pub cell_size: Option<String>,

    /// Play a video's sound in --preview mode, which is otherwise muted
    #[arg(long)]
    pub sound: bool,

    /// Show images scaled to fill the screen until any key is pressed
    #[arg(long, conflicts_with = "preview")]
    pub fullscreen: bool,
}
