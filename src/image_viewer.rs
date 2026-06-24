use crate::config::Config;
use crate::kitty;
use image::{AnimationDecoder, GenericImageView};
use std::fs::File;
use std::time::Duration;

const CELL_WIDTH_PX: u32 = 10;
const CELL_HEIGHT_PX: u32 = 20;
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
    term_cols: u16,
    term_rows: u16,
) -> ImageDisplaySize {
    let zoom = if zoom.is_finite() { zoom.max(0.0) } else { 0.0 };
    let desired_cols = ((image_width as f32 * zoom) / CELL_WIDTH_PX as f32)
        .round()
        .max(1.0) as u32;
    let desired_rows = ((image_height as f32 * zoom) / CELL_HEIGHT_PX as f32)
        .round()
        .max(1.0) as u32;
    let max_cols = u32::from(term_cols.saturating_sub(FIT_MARGIN_COLS).max(1));
    let max_rows = u32::from(term_rows.saturating_sub(FIT_MARGIN_ROWS).max(1));
    let scale = (max_cols as f32 / desired_cols as f32)
        .min(max_rows as f32 / desired_rows as f32)
        .min(1.0);
    let target_cols = ((desired_cols as f32 * scale).round() as u32)
        .max(1)
        .min(max_cols);
    let target_rows = ((desired_rows as f32 * scale).round() as u32)
        .max(1)
        .min(max_rows);

    ImageDisplaySize {
        target_cols,
        target_rows,
        target_w_px: target_cols * CELL_WIDTH_PX,
        target_h_px: target_rows * CELL_HEIGHT_PX,
    }
}

pub fn view(config: &Config, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Query terminal dimensions (columns, rows)
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // Load static image first (to determine dimensions/format)
    let img = image::open(file_path)?;
    let (img_w, img_h) = img.dimensions();

    // Calculate display dimensions in cells at original size, shrinking only to fit.
    let display_size = image_display_size(img_w, img_h, config.zoom, term_cols, term_rows);
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
        let size = image_display_size(640, 360, 1.0, 120, 40);

        assert_eq!(size.target_w_px, 640);
        assert_eq!(size.target_h_px, 360);
        assert_eq!(size.target_cols, 64);
        assert_eq!(size.target_rows, 18);
    }

    #[test]
    fn large_image_shrinks_to_fit_with_margin() {
        let size = image_display_size(1280, 720, 1.0, 80, 24);

        assert_eq!(size.target_cols, 76);
        assert_eq!(size.target_rows, 21);
        assert_eq!(size.target_w_px, 760);
        assert_eq!(size.target_h_px, 420);
    }
}
