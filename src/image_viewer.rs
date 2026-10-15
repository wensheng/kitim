use crate::config::Config;
use crate::kitty;
use crate::terminal_geometry::{cells_for_pixels, TerminalGeometry};
use crate::video_player::TerminalSession;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyEventKind},
    execute,
};
use image::imageops::FilterType;
use image::{AnimationDecoder, GenericImageView};
use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

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
    if config.fullscreen {
        return view_fullscreen(config, file_path);
    }
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

/// Where and how large a fullscreen image is drawn.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct FullscreenSize {
    target_cols: u32,
    target_rows: u32,
    target_w_px: u32,
    target_h_px: u32,
    origin_col: u16,
    origin_row: u16,
}

/// Largest size keeping the aspect ratio that fits the whole screen, with
/// the cell position to draw it centered. Unlike the inline path, images
/// are enlarged: filling the screen is the point.
fn fullscreen_display_size(image: (u32, u32), geometry: TerminalGeometry) -> FullscreenSize {
    let (image_width, image_height) = (image.0.max(1), image.1.max(1));
    let max_w_px = u32::from(geometry.cols) * geometry.cell_width_px;
    let max_h_px = u32::from(geometry.rows) * geometry.cell_height_px;
    let scale = (f64::from(max_w_px) / f64::from(image_width))
        .min(f64::from(max_h_px) / f64::from(image_height));
    let target_w_px = ((f64::from(image_width) * scale).round() as u32).clamp(1, max_w_px);
    let target_h_px = ((f64::from(image_height) * scale).round() as u32).clamp(1, max_h_px);
    let target_cols =
        cells_for_pixels(target_w_px, geometry.cell_width_px).min(u32::from(geometry.cols));
    let target_rows =
        cells_for_pixels(target_h_px, geometry.cell_height_px).min(u32::from(geometry.rows));
    FullscreenSize {
        target_cols,
        target_rows,
        target_w_px,
        target_h_px,
        origin_col: geometry
            .cols
            .saturating_sub(target_cols.min(u32::from(u16::MAX)) as u16)
            / 2,
        origin_row: geometry
            .rows
            .saturating_sub(target_rows.min(u32::from(u16::MAX)) as u16)
            / 2,
    }
}

/// What ended a fullscreen wait.
enum Interrupt {
    KeyPressed,
    Resized,
}

/// Sleeps `delay` (or forever when `None`), waking every 20 ms to check for
/// a key press or a resize. `Ok(None)` means the delay passed quietly.
fn interrupt_within(delay: Option<Duration>) -> std::io::Result<Option<Interrupt>> {
    const SLICE: Duration = Duration::from_millis(20);
    let deadline = delay.map(|d| Instant::now() + d);
    loop {
        let awake = match deadline {
            Some(at) => at
                .checked_duration_since(Instant::now())
                .unwrap_or_default(),
            None => SLICE,
        };
        if !event::poll(awake.min(SLICE))? {
            if deadline.is_some_and(|at| Instant::now() >= at) {
                return Ok(None);
            }
            continue;
        }
        return match event::read()? {
            Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                Ok(Some(Interrupt::KeyPressed))
            }
            Event::Resize(_, _) => Ok(Some(Interrupt::Resized)),
            _ => continue,
        };
    }
}

/// Shows one image (or a looping GIF) scaled to fill the screen, centered,
/// until any key is pressed. Redraws on resize.
fn view_fullscreen(config: &Config, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Decode before taking the screen, so errors don't flash it blank.
    let img = image::open(file_path)?;
    kitty::ensure_graphics_support()?;
    let frames = if image::guess_format(&std::fs::read(file_path)?)? == image::ImageFormat::Gif {
        image::codecs::gif::GifDecoder::new(std::io::BufReader::new(File::open(file_path)?))?
            .into_frames()
            .collect_frames()?
    } else {
        Vec::new()
    };
    let _session = TerminalSession::enter()?;
    let mut stdout = io::stdout();
    let mut tmux_state = kitty::TmuxImageState::new();
    loop {
        let geometry = TerminalGeometry::current();
        let size = fullscreen_display_size(img.dimensions(), geometry);
        if frames.is_empty() {
            let resized =
                img.resize_exact(size.target_w_px, size.target_h_px, FilterType::Triangle);
            let rgba = resized.to_rgba8();
            execute!(stdout, MoveTo(size.origin_col, size.origin_row))?;
            // C=1: the cursor stays put, so a full-width image can't scroll.
            kitty::write_rgba_frame(
                rgba.as_raw(),
                size.target_w_px,
                size.target_h_px,
                size.target_cols,
                size.target_rows,
                true,
            )?;
            if matches!(interrupt_within(None)?, Some(Interrupt::KeyPressed)) {
                return Ok(());
            }
        } else {
            'play: loop {
                for frame in &frames {
                    let frame_img = image::DynamicImage::ImageRgba8(frame.buffer().clone())
                        .resize_exact(size.target_w_px, size.target_h_px, FilterType::Nearest);
                    let rgba = frame_img.to_rgba8();
                    execute!(stdout, MoveTo(size.origin_col, size.origin_row))?;
                    kitty::write_rgba_frame_with_tmux_state(
                        rgba.as_raw(),
                        size.target_w_px,
                        size.target_h_px,
                        size.target_cols,
                        size.target_rows,
                        true,
                        &mut tmux_state,
                    )?;
                    let delay = match config.fps {
                        Some(fps) => Duration::from_secs_f32(1.0 / fps),
                        None => Duration::from(frame.delay()),
                    };
                    match interrupt_within(Some(delay))? {
                        Some(Interrupt::KeyPressed) => return Ok(()),
                        Some(Interrupt::Resized) => break 'play,
                        None => {}
                    }
                }
            }
        }
    }
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
    fn fullscreen_fills_the_screen_centered_and_upscaled() {
        // 80x24 cells of 10x20 px = 800x480 px; a 100x50 image scales 8x.
        let geometry = TerminalGeometry::from_cells(80, 24);
        let size = fullscreen_display_size((100, 50), geometry);
        assert_eq!(
            (
                size.target_w_px,
                size.target_h_px,
                size.target_cols,
                size.target_rows
            ),
            (800, 400, 80, 20)
        );
        assert_eq!((size.origin_col, size.origin_row), (0, 2));

        // A wide image is height-bound and pillarboxed: 800x200 px is 80x10
        // cells, centered vertically at row (24-10)/2.
        let size = fullscreen_display_size((1600, 400), geometry);
        assert_eq!((size.target_w_px, size.target_h_px), (800, 200));
        assert_eq!((size.origin_col, size.origin_row), (0, 7));

        // A tall image is width-bound and letterboxed: 48 px is 5 cells wide.
        let size = fullscreen_display_size((100, 1000), geometry);
        assert_eq!((size.target_w_px, size.target_h_px), (48, 480));
        assert_eq!((size.origin_col, size.origin_row), (37, 0));
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
