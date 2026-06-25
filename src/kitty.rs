use base64::{prelude::BASE64_STANDARD, Engine};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Once,
};
use std::thread;
use std::time::Duration;

const KITTY_CHUNK_SIZE: usize = 4096;
const TMUX_PASSTHROUGH_PREFIX: &[u8] = b"\x1bPtmux;";
const TMUX_PASSTHROUGH_SUFFIX: &[u8] = b"\x1b\\";
const KITTY_PLACEHOLDER: char = '\u{10EEEE}';
const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

static TMUX_PASSTHROUGH_INIT: Once = Once::new();
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(1);

/// Reusable image IDs for tmux video/GIF output. Reusing two IDs bounds terminal
/// image storage while keeping the previously visible frame alive until replaced.
#[derive(Debug, Clone)]
pub struct TmuxImageState {
    ids: [u32; 2],
    used: [bool; 2],
    next: usize,
}

impl TmuxImageState {
    pub fn new() -> Self {
        Self {
            ids: [next_image_id(), next_image_id()],
            used: [false; 2],
            next: 0,
        }
    }

    fn next_frame_id(&mut self) -> (u32, bool) {
        let index = self.next;
        self.next = (self.next + 1) % self.ids.len();

        let was_used = self.used[index];
        self.used[index] = true;
        (self.ids[index], was_used)
    }
}

/// Robustly write all bytes to stdout, retrying on EAGAIN / WouldBlock errors.
pub fn write_all_robust<W: Write>(mut writer: W, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ))
            }
            Ok(n) => buf = &buf[n..],
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(ref e) if e.raw_os_error() == Some(35) => {
                // EAGAIN on mac
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Robustly flush stdout, retrying on EAGAIN / WouldBlock errors.
pub fn flush_robust<W: Write>(mut writer: W) -> io::Result<()> {
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(ref e) if e.raw_os_error() == Some(35) => {
                // EAGAIN on mac
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
}

fn next_image_id() -> u32 {
    let id = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
    if id == 0 {
        next_image_id()
    } else {
        id
    }
}

fn tmux_passthrough_needed() -> bool {
    let Some(tmux_env) = std::env::var_os("TMUX") else {
        return false;
    };
    if tmux_env.as_os_str().is_empty() {
        return false;
    }

    TMUX_PASSTHROUGH_INIT.call_once(|| {
        let _ = Command::new("tmux")
            .args(["set", "-p", "allow-passthrough", "on"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });

    true
}

fn tmux_placeholder_cells(cols: u32, rows: u32) -> (u32, u32) {
    let max = DIACRITICS.len() as u32;
    (cols.clamp(1, max), rows.clamp(1, max))
}

fn append_tmux_wrapped_packet(buf: &mut Vec<u8>, packet: &[u8]) {
    buf.extend_from_slice(TMUX_PASSTHROUGH_PREFIX);
    for &byte in packet {
        if byte == b'\x1b' {
            buf.push(b'\x1b');
        }
        buf.push(byte);
    }
    buf.extend_from_slice(TMUX_PASSTHROUGH_SUFFIX);
}

fn write_kitty_packet<W: Write>(
    writer: &mut W,
    packet: &[u8],
    tmux_passthrough: bool,
) -> io::Result<()> {
    if !tmux_passthrough {
        return write_all_robust(writer, packet);
    }

    let mut wrapped = Vec::with_capacity(packet.len() + 16);
    append_tmux_wrapped_packet(&mut wrapped, packet);
    write_all_robust(writer, &wrapped)
}

fn write_tmux_delete_image<W: Write>(writer: &mut W, image_id: u32) -> io::Result<()> {
    let mut packet = Vec::new();
    write!(packet, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", image_id)?;
    write_kitty_packet(writer, &packet, true)
}

fn append_diacritic(buf: &mut Vec<u8>, value: u32) {
    if let Some(ch) = DIACRITICS.get(value as usize) {
        let mut encoded = [0; 4];
        buf.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
    }
}

fn append_tmux_placeholders(
    buf: &mut Vec<u8>,
    image_id: u32,
    cols: u32,
    rows: u32,
    indent_cols: u16,
    restore_cursor: bool,
) -> io::Result<()> {
    let (cols, rows) = tmux_placeholder_cells(cols, rows);
    if restore_cursor {
        buf.extend_from_slice(b"\x1b7");
    }

    buf.extend_from_slice(b"\r");
    write!(
        buf,
        "\x1b[38:2:{}:{}:{}m",
        (image_id >> 16) & 0xff,
        (image_id >> 8) & 0xff,
        image_id & 0xff
    )?;

    for row in 0..rows {
        if indent_cols > 0 {
            write!(buf, "\x1b[{}C", indent_cols)?;
        }
        for col in 0..cols {
            let mut encoded = [0; 4];
            buf.extend_from_slice(KITTY_PLACEHOLDER.encode_utf8(&mut encoded).as_bytes());
            append_diacritic(buf, row);
            append_diacritic(buf, col);
        }
        if row + 1 < rows {
            buf.extend_from_slice(b"\x1b[39m\n\r");
            write!(
                buf,
                "\x1b[38:2:{}:{}:{}m",
                (image_id >> 16) & 0xff,
                (image_id >> 8) & 0xff,
                image_id & 0xff
            )?;
        }
    }

    buf.extend_from_slice(b"\x1b[39m");
    if restore_cursor {
        buf.extend_from_slice(b"\x1b8");
    }
    Ok(())
}

/// Move cursor up robustly without direct stdout blocking.
pub fn move_up_robust(rows: u16) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut buf = Vec::new();
    crossterm::queue!(
        buf,
        crossterm::cursor::MoveUp(rows),
        crossterm::cursor::MoveToColumn(0)
    )?;
    write_all_robust(&mut stdout, &buf)?;
    flush_robust(&mut stdout)?;
    Ok(())
}

/// Write a 32-bit RGBA image to stdout using the Kitty graphics protocol.
/// `pixels` is the raw RGBA pixel buffer.
/// `width_px` and `height_px` are the image dimensions in pixels.
/// `cols` and `rows` are the occupied terminal cells, used for tmux placeholders.
/// `prevent_cursor_move` instructs the terminal whether to keep the cursor fixed (C=1).
pub fn write_rgba_frame(
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write_rgba_frame_to(
        &mut stdout,
        pixels,
        width_px,
        height_px,
        cols,
        rows,
        prevent_cursor_move,
    )
}

pub fn write_rgba_frame_with_tmux_state(
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
    tmux_image_state: &mut TmuxImageState,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write_rgba_frame_to_with_tmux_state(
        &mut stdout,
        pixels,
        width_px,
        height_px,
        cols,
        rows,
        prevent_cursor_move,
        0,
        Some(tmux_image_state),
    )
}

pub fn write_rgba_frame_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    write_rgba_frame_to_with_tmux_state(
        writer,
        pixels,
        width_px,
        height_px,
        cols,
        rows,
        prevent_cursor_move,
        0,
        None,
    )
}

pub fn write_rgba_frame_to_with_tmux_state<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
    placeholder_indent_cols: u16,
    tmux_image_state: Option<&mut TmuxImageState>,
) -> io::Result<()> {
    let tmux_passthrough = tmux_passthrough_needed();
    let (image_id, delete_before_transmit) = if tmux_passthrough {
        tmux_image_state
            .map(TmuxImageState::next_frame_id)
            .unwrap_or_else(|| (next_image_id(), false))
    } else {
        (0, false)
    };
    let (placeholder_cols, placeholder_rows) = tmux_placeholder_cells(cols, rows);

    if delete_before_transmit {
        write_tmux_delete_image(writer, image_id)?;
    }

    let base64_str = BASE64_STANDARD.encode(pixels);
    let bytes = base64_str.as_bytes();
    let mut offset = 0;

    while offset < bytes.len() {
        let is_last = offset + KITTY_CHUNK_SIZE >= bytes.len();
        let chunk = &bytes[offset..std::cmp::min(offset + KITTY_CHUNK_SIZE, bytes.len())];
        let m_param = if is_last { 0 } else { 1 };

        let mut packet = Vec::new();
        if offset == 0 {
            // First chunk: specify action (a=T), format (f=32 for RGBA), dimensions, quiet mode (q=2)
            // and optional cursor movement policy (C=1 to prevent cursor movement)
            let c_policy = if prevent_cursor_move { ",C=1" } else { "" };
            if tmux_passthrough {
                write!(
                    packet,
                    "\x1b_Ga=T,i={},f=32,s={},v={},c={},r={},U=1{},q=2,m={};",
                    image_id,
                    width_px,
                    height_px,
                    placeholder_cols,
                    placeholder_rows,
                    c_policy,
                    m_param
                )?;
            } else {
                write!(
                    packet,
                    "\x1b_Ga=T,f=32,s={},v={}{},q=2,m={};",
                    width_px, height_px, c_policy, m_param
                )?;
            }
        } else {
            // Subsequent chunks: only specify more parameter (m) and quiet mode (q=2)
            write!(packet, "\x1b_Gq=2,m={};", m_param)?;
        }

        packet.write_all(chunk)?;
        packet.write_all(b"\x1b\\")?;

        write_kitty_packet(&mut *writer, &packet, tmux_passthrough)?;
        offset += KITTY_CHUNK_SIZE;
    }

    if tmux_passthrough {
        let mut placeholders = Vec::new();
        append_tmux_placeholders(
            &mut placeholders,
            image_id,
            cols,
            rows,
            placeholder_indent_cols,
            prevent_cursor_move,
        )?;
        write_all_robust(&mut *writer, &placeholders)?;
    }

    flush_robust(&mut *writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_wrapper_doubles_escapes_inside_dcs_passthrough() {
        let mut wrapped = Vec::new();
        append_tmux_wrapped_packet(&mut wrapped, b"\x1b_Gpayload\x1b\\");

        assert_eq!(wrapped, b"\x1bPtmux;\x1b\x1b_Gpayload\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn tmux_placeholders_encode_image_id_and_cell_coordinates() {
        let mut placeholders = Vec::new();
        append_tmux_placeholders(&mut placeholders, 0x00010203, 2, 2, 3, true).unwrap();
        let placeholders = String::from_utf8(placeholders).unwrap();

        assert!(placeholders.starts_with("\x1b7\r\x1b[38:2:1:2:3m\x1b[3C"));
        assert!(placeholders.contains("\u{10EEEE}\u{0305}\u{0305}"));
        assert!(placeholders.contains("\u{10EEEE}\u{030D}\u{030D}"));
        assert!(placeholders.ends_with("\x1b[39m\x1b8"));
    }

    #[test]
    fn tmux_placeholder_cells_are_capped_to_available_diacritics() {
        assert_eq!(tmux_placeholder_cells(999, 999), (297, 297));
        assert_eq!(tmux_placeholder_cells(0, 0), (1, 1));
    }

    #[test]
    fn direct_kitty_packet_uses_pixel_size_without_cell_placement() {
        if std::env::var_os("TMUX").is_some() {
            return;
        }

        let mut out = Vec::new();
        write_rgba_frame_to(&mut out, &[0, 0, 0, 255], 1, 1, 1, 1, false).unwrap();
        let packet = String::from_utf8(out).unwrap();

        assert!(packet.starts_with("\x1b_Ga=T,f=32,s=1,v=1,q=2,m=0;"));
        assert!(!packet.contains(",c="));
        assert!(!packet.contains(",r="));
    }
}
