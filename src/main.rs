mod audio_player;
mod config;
mod ffmpeg;
mod image_viewer;
mod kitty;
mod playback_control;
mod terminal_geometry;
mod video_player;

use clap::Parser;
use config::Config;
use std::path::Path;

fn main() {
    let config = Config::parse();

    for file in &config.files {
        let path = Path::new(file);
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Common video formats
        let is_video_ext = matches!(
            ext.as_str(),
            "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" | "3gp"
        );

        // Common audio formats
        let is_audio_ext = matches!(
            ext.as_str(),
            "mp3"
                | "m4a"
                | "aac"
                | "wav"
                | "flac"
                | "ogg"
                | "opus"
                | "wma"
                | "alac"
                | "aiff"
                | "aif"
        );

        let result = if is_video_ext {
            video_player::play(&config, file)
        } else if is_audio_ext {
            audio_player::play(&config, file)
        } else {
            // Try as image, then fall back through FFmpeg-backed video and audio decoders.
            match image_viewer::view(&config, file) {
                Ok(_) => Ok(()),
                Err(error) if kitty::is_graphics_support_error(error.as_ref()) => Err(error),
                Err(_) => match video_player::play(&config, file) {
                    Ok(_) => Ok(()),
                    Err(error) if kitty::is_graphics_support_error(error.as_ref()) => Err(error),
                    Err(_) => audio_player::play(&config, file),
                },
            }
        };

        if let Err(e) = result {
            eprintln!("Error processing {}: {}", file, e);
        }
    }
}
