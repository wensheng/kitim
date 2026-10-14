# kitim

**Turn your terminal into a fast, focused media viewer for images, audio, and videos without leaving the command line.**

(Part of kit* series of graphic terminal apps:
 [kitim](https://github.com/wensheng/kitim)
 [kitmd](https://github.com/wensheng/kitmd)
 [kitpdf](https://github.com/wensheng/kitpdf)
 [kitdraw](https://github.com/wensheng/kitdraw)
 [kitDOOM](https://github.com/wensheng/kitdoom))

***kitim runs on terminals that supports the Kitty graphics protocol:
[**Ghostty**](https://ghostty.org/),
[**Kitty**](https://sw.kovidgoyal.net/kitty/),
[**WezTerm**](https://wezterm.net/),
[**cmux**](https://github.com/manaflow-ai/cmux),
[**Warp**](https://warp.dev/),
[**iTerm2**](https://www.iterm2.com/) (see notes below for iTerm2).***

[![demo]](https://github.com/user-attachments/assets/d21faeb7-aead-41c1-b270-0f366005a2d6)

## Install

    cargo install kitim

(Audio and video playback require `ffmpeg` installed, `brew install ffmpeg` on MacOS)

Windows MSVC builds require a **shared FFmpeg development SDK**, including `include`,
`lib`, and `bin` directories. An `ffmpeg.exe` installation alone is insufficient.
Set `FFMPEG_DIR` to the SDK root, or use vcpkg:

```powershell
vcpkg install ffmpeg:x64-windows
$env:VCPKG_ROOT = "C:/path/to/vcpkg"
cargo build --release
```

The build uses the target architecture's Windows triplet by default;
`VCPKG_TARGET_TRIPLET` or `VCPKG_DEFAULT_TRIPLET` can override it. It detects the
FFmpeg ABI from the SDK headers and copies runtime DLLs beside the executable.
When distributing the executable, include those DLLs.

---

## Why

- Stop bouncing from terminal to Finder, Preview, or a browser just to inspect a file.
- Preview images, animated GIFs, videos, and audio in the same place you already work.
- Keep media checks scriptable, keyboard-first, and fast enough for real terminal workflows.

---

<!-- ## Show, Don't Tell

![kitim demo placeholder](./assets/demo.gif)
-->

## Key Capabilities

- **Inline visual previews** powered by the Kitty graphics protocol, so media appears directly in your terminal.
- **Video playback with synced audio** using FFmpeg decoding, CPAL output, and a render thread tuned for smooth playback.
- **Audio-only playback** for common formats such as MP3, M4A, AAC, WAV, FLAC, OGG, and Opus.
- **Tiny controls, useful defaults**: zoom output, override playback FPS, and loop videos, GIFs, or audio when you need to inspect media.

---

## Usage

```bash
kitim screenshot.png
kitim song.mp3
kitim -z 1.5 clip.webm
kitim --loop --frame-rate 24 animation.gif
```

`-z`/`--zoom` is a multiplier on the original media size. Media displays at
original size by default, and shrinks to fit the terminal with a small margin
when it would otherwise exceed the available screen area.

### Preview mode

```bash
kitim --preview photo.jpg 60 30
```

`--preview FILE COLS ROWS` renders one file into a COLS x ROWS cell area of
another program's screen, such as a file manager's preview pane. It is how
[vfm](https://github.com/wensheng/vfm) previews images and videos. kitim then
writes kitty graphics to stdout and does no other terminal I/O: no raw mode,
no queries, no input, no audio. The host moves the cursor to the area's
top-left cell and forwards the output.

Each frame is a cursor move that centers the image, then one image with a
fresh `i=` id, `C=1` (the cursor stays put) and `q=2` (no replies). Images
keep their size, shrinking to fit; the terminal scales each to its cells, so
animation frames are sent at reduced resolution. Still images print one frame
and exit. GIFs and videos loop until the host kills kitim or closes the
pipe. Videos are muted unless `--sound` is given: then their audio plays on
the default output device and paces the picture. The first frame shows
before the device opens, which can take a few hundred milliseconds. If the
file can't be shown, kitim prints a one-line reason instead and exits with
status 1.

The cell size in pixels comes from the tty driver, else defaults to 10x20;
`--cell-size WxH` overrides it.

---

## How It Works

```text
file path -> image/GIF decoder or FFmpeg -> RGBA frames -> Kitty graphics chunks -> terminal
                                      audio -> resampler -> CPAL output
                                      video audio -> resampler -> CPAL output -> sync clock
```

`kitim` keeps the rendering path direct: decode into RGBA, resize to terminal cell geometry, chunk through the Kitty protocol, and use audio timing as the playback clock when a video has sound.

At end of input, kitim drains both FFmpeg decoders and the audio resampler, then
waits for queued video and audio to finish before exiting or looping. Seeking and
quitting remain available while that final media is playing.

The opt-in playback regression uses Python 3, `ffmpeg`, and `ffprobe` to generate
short B-frame/AAC clips and check actual terminal output, final frames, looping,
and input near EOF:

```sh
python tests/video_playback.py --binary target/release/kitim
```

On Windows, use `target/release/kitim.exe` and add `--conpty PATH/conpty.dll`,
pointing at Vivido's bundled runtime with its matching `OpenConsole.exe` beside it.
Add `--media PATH` to check another video without modifying it.

## Notes

For iTerm2, set:

```bash
export TERMINAL_KITTEN_GRAPHICS=1
```

in your `.zshrc` or `.bashrc`.
