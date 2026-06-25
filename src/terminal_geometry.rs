use std::time::Duration;

const DEFAULT_CELL_WIDTH_PX: u32 = 10;
const DEFAULT_CELL_HEIGHT_PX: u32 = 20;
const CSI_CELL_SIZE_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct TerminalGeometry {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_width_px: u32,
    pub(crate) cell_height_px: u32,
}

impl TerminalGeometry {
    pub(crate) fn current() -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::current_with_reported_cells(cols, rows)
    }

    pub(crate) fn current_with_reported_cells(cols: u16, rows: u16) -> Self {
        let mut cols = cols.max(1);
        let mut rows = rows.max(1);

        if let Some(winsize) = ioctl_winsize() {
            cols = winsize.cols.max(1);
            rows = winsize.rows.max(1);
            if let Some((cell_width_px, cell_height_px)) = winsize.cell_size() {
                return Self::with_cell_size(cols, rows, cell_width_px, cell_height_px);
            }
        }

        if let Some((cell_width_px, cell_height_px)) = query_cell_size_with_csi() {
            return Self::with_cell_size(cols, rows, cell_width_px, cell_height_px);
        }

        Self::from_cells(cols, rows)
    }

    pub(crate) fn from_cells(cols: u16, rows: u16) -> Self {
        Self::with_cell_size(cols, rows, DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX)
    }

    pub(crate) fn with_cell_size(
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
            cell_width_px: cell_width_px.max(1),
            cell_height_px: cell_height_px.max(1),
        }
    }

    pub(crate) fn drawable_width_px(self, margin_cols: u16) -> u32 {
        u32::from(self.cols.saturating_sub(margin_cols).max(1)) * self.cell_width_px
    }

    pub(crate) fn drawable_height_px(self, margin_rows: u16) -> u32 {
        u32::from(self.rows.saturating_sub(margin_rows).max(1)) * self.cell_height_px
    }
}

pub(crate) fn cells_for_pixels(pixels: u32, cell_px: u32) -> u32 {
    let cell_px = cell_px.max(1);
    pixels.max(1).saturating_add(cell_px - 1) / cell_px
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct RawWinsize {
    cols: u16,
    rows: u16,
    xpixel: u32,
    ypixel: u32,
}

impl RawWinsize {
    fn cell_size(self) -> Option<(u32, u32)> {
        let cols = u32::from(self.cols);
        let rows = u32::from(self.rows);
        if cols == 0 || rows == 0 {
            return None;
        }

        // Match timg's sanity check: bogus terminals often leave pixel fields
        // empty or too small to be real screen pixel dimensions.
        if self.xpixel >= 2 * cols && self.ypixel >= 4 * rows {
            Some(((self.xpixel / cols).max(1), (self.ypixel / rows).max(1)))
        } else {
            None
        }
    }
}

#[cfg(unix)]
fn ioctl_winsize() -> Option<RawWinsize> {
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO, libc::STDIN_FILENO] {
        if unsafe { libc::isatty(fd) } != 1 {
            continue;
        }

        let mut winsize = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, winsize.as_mut_ptr()) } != 0 {
            continue;
        }

        let winsize = unsafe { winsize.assume_init() };
        if winsize.ws_col == 0 || winsize.ws_row == 0 {
            continue;
        }

        return Some(RawWinsize {
            cols: winsize.ws_col,
            rows: winsize.ws_row,
            xpixel: u32::from(winsize.ws_xpixel),
            ypixel: u32::from(winsize.ws_ypixel),
        });
    }

    None
}

#[cfg(not(unix))]
fn ioctl_winsize() -> Option<RawWinsize> {
    None
}

#[cfg(unix)]
fn query_cell_size_with_csi() -> Option<(u32, u32)> {
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    struct TermiosRestore {
        fd: libc::c_int,
        original: libc::termios,
    }

    impl Drop for TermiosRestore {
        fn drop(&mut self) {
            let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original) };
        }
    }

    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let fd = tty.as_raw_fd();

    let mut original = std::mem::MaybeUninit::<libc::termios>::zeroed();
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return None;
    }
    let original = unsafe { original.assume_init() };

    let mut raw = original;
    raw.c_iflag = 0;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 0;

    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    let _restore = TermiosRestore { fd, original };

    tty.write_all(b"\x1b[16t").ok()?;
    tty.flush().ok()?;

    let deadline = Instant::now() + CSI_CELL_SIZE_TIMEOUT;
    let mut response = Vec::with_capacity(128);
    let mut buf = [0_u8; 128];

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready <= 0 {
            break;
        }

        match tty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                if let Some(cell_size) = parse_csi_16t_response(&response) {
                    return Some(cell_size);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    parse_csi_16t_response(&response)
}

#[cfg(not(unix))]
fn query_cell_size_with_csi() -> Option<(u32, u32)> {
    None
}

fn parse_csi_16t_response(data: &[u8]) -> Option<(u32, u32)> {
    const PREFIX: &str = "\x1b[6;";

    let text = std::str::from_utf8(data).ok()?;
    for (start, _) in text.match_indices(PREFIX) {
        let rest = &text[start + PREFIX.len()..];
        let end = rest.find('t')?;
        let mut parts = rest[..end].split(';');
        let height = parts.next()?.parse::<u32>().ok()?;
        let width = parts.next()?.parse::<u32>().ok()?;
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_for_pixels_rounds_up_to_occupied_cells() {
        assert_eq!(cells_for_pixels(640, 10), 64);
        assert_eq!(cells_for_pixels(641, 10), 65);
        assert_eq!(cells_for_pixels(0, 10), 1);
    }

    #[test]
    fn parses_csi_16t_cell_size_response() {
        assert_eq!(parse_csi_16t_response(b"noise\x1b[6;19;9t"), Some((9, 19)));
    }

    #[test]
    fn rejects_implausible_ioctl_pixel_sizes() {
        let winsize = RawWinsize {
            cols: 80,
            rows: 24,
            xpixel: 80,
            ypixel: 24,
        };

        assert_eq!(winsize.cell_size(), None);
    }
}
