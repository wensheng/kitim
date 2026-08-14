use crate::config::Config;
use crate::kitty;
use crate::terminal_geometry::{cells_for_pixels, TerminalGeometry};
use image::{AnimationDecoder, GenericImageView};
use std::fs::File;
use std::time::Duration;

const FIT_MARGIN_COLS: u16 = 4;
const FIT_MARGIN_ROWS: u16 = 2;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct ImageDisplaySize {
    target_cols: u32,
    target_rows: u32,
    target_w_px: u32,
    target_h_px: u32,
}

fn image_display_size(
    image_width: u32,
    image_height: u32,
    zoom: f32,
    geometry: TerminalGeometry,
) -> ImageDisplaySize {
    let zoom = if zoom.is_finite() { zoom.max(0.0) } else { 0.0 };
    let desired_w_px = (image_width as f32 * zoom).round().max(1.0) as u32;
    let desired_h_px = (image_height as f32 * zoom).round().max(1.0) as u32;
    let max_w_px = geometry.drawable_width_px(FIT_MARGIN_COLS);
    let max_h_px = geometry.drawable_height_px(FIT_MARGIN_ROWS);
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

    ImageDisplaySize {
        target_cols: cells_for_pixels(target_w_px, geometry.cell_width_px),
        target_rows: cells_for_pixels(target_h_px, geometry.cell_height_px),
        target_w_px,
        target_h_px,
    }
}

pub fn view(config: &Config, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Load static image first (to determine dimensions/format)
    let img = image::open(file_path)?;
    kitty::ensure_graphics_support()?;
    // Query terminal cells and pixel geometry. The Kitty graphics protocol uses
    // real pixels, so cell dimensions must not be guessed.
    let terminal_geometry = TerminalGeometry::current();
    let (img_w, img_h) = img.dimensions();

    // Calculate display dimensions in cells at original size, shrinking only to fit.
    let display_size = image_display_size(img_w, img_h, config.zoom, terminal_geometry);
    let target_cols = display_size.target_cols;
    let target_rows = display_size.target_rows;
    let target_w_px = display_size.target_w_px;
    let target_h_px = display_size.target_h_px;

    // Check if the file is an animated GIF
    let file = File::open(file_path)?;
    let is_gif = image::guess_format(&std::fs::read(file_path)?)? == image::ImageFormat::Gif;

    if is_gif {
        let decoder = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file))?;
        let frames: Vec<_> = decoder.into_frames().collect_frames()?;

        // Pre-allocate terminal rows to prevent scrolling drift during playback
        for _ in 0..target_rows {
            println!();
        }
        kitty::move_up_robust(target_rows as u16)?;

        let mut tmux_image_state = kitty::TmuxImageState::new();
        'outer: loop {
            for frame in &frames {
                let buffer: &image::RgbaImage = frame.buffer();
                let frame_img = image::DynamicImage::ImageRgba8(buffer.clone());
                let resized = frame_img.resize_exact(
                    target_w_px,
                    target_h_px,
                    image::imageops::FilterType::Nearest,
                );
                let rgba = resized.to_rgba8();

                // Draw using the Kitty protocol with prevent_cursor_move = true
                kitty::write_rgba_frame_with_tmux_state(
                    &rgba,
                    target_w_px,
                    target_h_px,
                    target_cols,
                    target_rows,
                    true,
                    &mut tmux_image_state,
                )?;

                let delay = match config.fps {
                    Some(fps) => Duration::from_secs_f32(1.0 / fps),
                    None => Duration::from(frame.delay()),
                };
                std::thread::sleep(delay);
            }

            if !config.loop_video {
                break 'outer;
            }
        }

        // Move cursor to bottom-left after GIF ends
        let mut stdout = std::io::stdout().lock();
        let mut buf = Vec::new();
        let _ = crossterm::queue!(
            buf,
            crossterm::cursor::MoveDown(target_rows as u16),
            crossterm::cursor::MoveToColumn(0)
        );
        let _ = kitty::write_all_robust(&mut stdout, &buf);
        let _ = kitty::flush_robust(&mut stdout);
        println!();
    } else {
        // Static image
        let resized = img.resize_exact(
            target_w_px,
            target_h_px,
            image::imageops::FilterType::Triangle,
        );
        let rgba = resized.to_rgba8();

        kitty::write_rgba_frame(
            &rgba,
            target_w_px,
            target_h_px,
            target_cols,
            target_rows,
            false,
        )?;
        println!(); // Add trailing newline for static output
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_image_zoom_uses_original_size() {
        let geometry = TerminalGeometry::from_cells(120, 40);
        let size = image_display_size(640, 360, 1.0, geometry);

        assert_eq!(size.target_w_px, 640);
        assert_eq!(size.target_h_px, 360);
        assert_eq!(size.target_cols, 64);
        assert_eq!(size.target_rows, 18);
    }

    #[test]
    fn large_image_shrinks_to_fit_with_margin() {
        let geometry = TerminalGeometry::from_cells(80, 24);
        let size = image_display_size(1280, 720, 1.0, geometry);

        assert_eq!(size.target_cols, 76);
        assert_eq!(size.target_rows, 22);
        assert_eq!(size.target_w_px, 760);
        assert_eq!(size.target_h_px, 428);
    }

    #[test]
    fn non_default_cell_geometry_preserves_pixel_size() {
        let geometry = TerminalGeometry::with_cell_size(120, 40, 12, 20);
        let size = image_display_size(640, 360, 1.0, geometry);

        assert_eq!(size.target_w_px, 640);
        assert_eq!(size.target_h_px, 360);
        assert_eq!(size.target_cols, 54);
        assert_eq!(size.target_rows, 18);
    }
}
