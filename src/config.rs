use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "viewim",
    version,
    about = "Kitty graphics-based terminal image, audio, and video player"
)]
pub struct Config {
    /// The image, audio, or video files to display/play
    #[arg(required = true)]
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

    /// Initial playback volume percentage
    #[arg(short = 'v', long = "volume", default_value_t = 100)]
    pub volume: u32,

    /// Disable all playback controls & control tips
    #[arg(short = 'd', long = "disable-controls")]
    pub disable_controls: bool,
}
