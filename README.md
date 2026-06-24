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

[![demo]](https://github.com/user-attachments/assets/acd301e1-b749-446a-ab40-67845a13a6bb)

## Install

    cargo install kitim

(Audio and video playback require `ffmpeg` installed, `brew install ffmpeg` on MacOS)

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

---

## How It Works

```text
file path -> image/GIF decoder or FFmpeg -> RGBA frames -> Kitty graphics chunks -> terminal
                                      audio -> resampler -> CPAL output
                                      video audio -> resampler -> CPAL output -> sync clock
```

`kitim` keeps the rendering path direct: decode into RGBA, resize to terminal cell geometry, chunk through the Kitty protocol, and use audio timing as the playback clock when a video has sound.

## Notes

For iTerm2, set:

```bash
export TERMINAL_KITTEN_GRAPHICS=1
```

in your `.zshrc` or `.bashrc`.
