"""Opt-in real-terminal EOF regression (Python stdlib, ffmpeg, ffprobe).

python tests/video_playback.py --binary target/release/kitim.exe --conpty PATH/conpty.dll
On macOS/Linux omit --conpty. Tests silent B-frame video and video with AAC audio.
An optional --media also checks an existing clip without modifying it.
"""
import argparse
import base64
import ctypes as c
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import tempfile
import threading
import time


class WindowsTerminal:
    def __init__(self, argv, dll):
        from ctypes import wintypes as w

        class Coord(c.Structure):
            _fields_ = [("x", c.c_short), ("y", c.c_short)]

        class Startup(c.Structure):
            _fields_ = [("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR),
                        ("title", w.LPWSTR), ("x", w.DWORD), ("y", w.DWORD),
                        ("xs", w.DWORD), ("ys", w.DWORD), ("xc", w.DWORD),
                        ("yc", w.DWORD), ("fill", w.DWORD), ("flags", w.DWORD),
                        ("show", w.WORD), ("size", w.WORD), ("data", c.c_void_p),
                        ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE)]

        class StartupEx(c.Structure):
            _fields_ = [("startup", Startup), ("attributes", c.c_void_p)]

        class Process(c.Structure):
            _fields_ = [("handle", w.HANDLE), ("thread", w.HANDLE),
                        ("pid", w.DWORD), ("tid", w.DWORD)]

        self.k = c.WinDLL("kernel32", use_last_error=True)
        self.console_api = c.WinDLL(str(Path(dll).resolve()), use_last_error=True)
        signatures = {
            "CreatePipe": ([c.POINTER(w.HANDLE), c.POINTER(w.HANDLE), c.c_void_p, w.DWORD], w.BOOL),
            "InitializeProcThreadAttributeList": ([c.c_void_p, w.DWORD, w.DWORD, c.POINTER(c.c_size_t)], w.BOOL),
            "UpdateProcThreadAttribute": ([c.c_void_p, w.DWORD, c.c_size_t, c.c_void_p, c.c_size_t, c.c_void_p, c.c_void_p], w.BOOL),
            "CreateProcessW": ([w.LPCWSTR, w.LPWSTR, c.c_void_p, c.c_void_p, w.BOOL, w.DWORD, c.c_void_p, w.LPCWSTR, c.POINTER(StartupEx), c.POINTER(Process)], w.BOOL),
            "ReadFile": ([w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL),
            "WriteFile": ([w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL),
            "WaitForSingleObject": ([w.HANDLE, w.DWORD], w.DWORD),
            "GetExitCodeProcess": ([w.HANDLE, c.POINTER(w.DWORD)], w.BOOL),
            "TerminateProcess": ([w.HANDLE, w.UINT], w.BOOL),
            "CloseHandle": ([w.HANDLE], w.BOOL),
            "DeleteProcThreadAttributeList": ([c.c_void_p], None),
        }
        for name, (args, result) in signatures.items():
            getattr(self.k, name).argtypes, getattr(self.k, name).restype = args, result
        self.console_api.ConptyCreatePseudoConsole.argtypes = [Coord, w.HANDLE, w.HANDLE, w.DWORD, c.POINTER(w.HANDLE)]
        self.console_api.ConptyCreatePseudoConsole.restype = c.c_long
        self.console_api.ConptyClosePseudoConsole.argtypes = [w.HANDLE]
        self.console_api.ConptyClosePseudoConsole.restype = None
        self.input, self.output, self.console = w.HANDLE(), w.HANDLE(), w.HANDLE()
        read_in, write_out = w.HANDLE(), w.HANDLE()
        self.check(self.k.CreatePipe(c.byref(read_in), c.byref(self.input), None, 0))
        self.check(self.k.CreatePipe(c.byref(self.output), c.byref(write_out), None, 0))
        assert self.console_api.ConptyCreatePseudoConsole(Coord(80, 24), read_in, write_out, 0, c.byref(self.console)) == 0
        self.k.CloseHandle(read_in)
        self.k.CloseHandle(write_out)
        size = c.c_size_t()
        self.k.InitializeProcThreadAttributeList(None, 1, 0, c.byref(size))
        self.attributes = c.create_string_buffer(size.value)
        self.check(self.k.InitializeProcThreadAttributeList(self.attributes, 1, 0, c.byref(size)))
        self.check(self.k.UpdateProcThreadAttribute(self.attributes, 0, 0x20016, self.console, c.sizeof(self.console), None, None))
        startup = StartupEx()
        startup.startup.cb, startup.startup.flags = c.sizeof(startup), 0x100
        startup.attributes = c.cast(self.attributes, c.c_void_p)
        self.process = Process()
        command = c.create_unicode_buffer(subprocess.list2cmdline(argv))
        self.check(self.k.CreateProcessW(None, command, None, None, False, 0x80000, None, None, c.byref(startup), c.byref(self.process)))

    @staticmethod
    def check(ok):
        if not ok:
            raise c.WinError(c.get_last_error())

    def read(self):
        from ctypes import wintypes as w
        buffer, count = c.create_string_buffer(65536), w.DWORD()
        return buffer.raw[:count.value] if self.k.ReadFile(self.output, buffer, len(buffer), c.byref(count), None) else b""

    def send(self, data):
        from ctypes import wintypes as w
        count = w.DWORD()
        self.check(self.k.WriteFile(self.input, data, len(data), c.byref(count), None))

    def poll(self):
        from ctypes import wintypes as w
        if self.k.WaitForSingleObject(self.process.handle, 0) != 0:
            return None
        status = w.DWORD()
        self.check(self.k.GetExitCodeProcess(self.process.handle, c.byref(status)))
        return status.value

    def close(self):
        if self.poll() is None:
            self.k.TerminateProcess(self.process.handle, 1)
        self.k.CloseHandle(self.process.thread)
        self.k.CloseHandle(self.process.handle)
        self.k.CloseHandle(self.input)
        self.console_api.ConptyClosePseudoConsole(self.console)
        self.k.CloseHandle(self.output)
        self.k.DeleteProcThreadAttributeList(self.attributes)


class UnixTerminal:
    def __init__(self, argv, _):
        import fcntl
        import pty
        import struct
        import termios
        self.fd, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 800, 480))

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

        self.process = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_terminal)
        os.close(slave)

    def read(self):
        try:
            return os.read(self.fd, 65536)
        except OSError:
            return b""

    def send(self, data):
        os.write(self.fd, data)

    def poll(self):
        return self.process.poll()

    def close(self):
        if self.poll() is None:
            self.process.kill()
        self.process.wait()
        os.close(self.fd)


def play(binary, media, dll, expected_frames=None, final_red=False, loop=False,
         actions=(), minimum_duration=None, maximum_duration=None):
    metadata = json.loads(subprocess.check_output([
        "ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "json", str(media)]))
    duration = float(metadata["format"]["duration"])
    # Clear multiplexer hints so the harness receives ordinary direct Kitty commands.
    environment = {key: os.environ.pop(key) for key in ("TMUX", "TERM_PROGRAM") if key in os.environ}
    start = time.monotonic()
    try:
        argv = [str(binary)] + (["--loop"] if loop else []) + [str(media)]
        terminal = (WindowsTerminal if os.name == "nt" else UnixTerminal)(argv, dll)
    finally:
        os.environ.update(environment)
    chunks = queue.Queue(maxsize=64)

    def reader():
        while data := terminal.read():
            chunks.put(data)
        chunks.put(None)

    threading.Thread(target=reader, daemon=True).start()
    pending, pixels, last_pixels = bytearray(), bytearray(), b""
    frames, red_frames, last_second = 0, 0, 0
    has_actions = bool(actions)
    actions = iter(actions)
    action = next(actions, None)
    is_red = lambda rgba: bool(rgba) and all(
        rgba[i] > 220 and rgba[i+1] < 35 and rgba[i+2] < 35 for i in range(0, len(rgba), 4))
    try:
        while True:
            assert time.monotonic() - start < duration + 20, "playback did not finish"
            if action and time.monotonic() - start >= action[0]:
                terminal.send(action[1])
                action = next(actions, None)
            try:
                data = chunks.get(timeout=0.1)
            except queue.Empty:
                if terminal.poll() is not None:
                    break
                continue
            if data is None:
                break
            pending.extend(data)
            for minutes, seconds in re.findall(rb"(\d+):(\d{2}) /", pending):
                last_second = max(last_second, int(minutes) * 60 + int(seconds))
            for query, reply in [(b"\x1b[6n", b"\x1b[1;1R"), (b"\x1b[16t", b"\x1b[6;20;10t")]:
                if query in pending:
                    terminal.send(reply)
            while match := re.search(rb"\x1b_G([^;]*);([^\x1b]*)\x1b\\", pending):
                fields = dict(item.split(b"=", 1) for item in match[1].split(b",") if b"=" in item)
                if fields.get(b"a") == b"T":
                    pixels.clear()
                pixels.extend(base64.b64decode(match[2]))
                if fields.get(b"m") == b"0":
                    frames += 1
                    last_pixels = bytes(pixels)
                    red_frames += is_red(last_pixels)
                del pending[:match.end()]
            # Retain only an incomplete command; never retain the entire video.
            marker = pending.find(b"\x1b_G")
            if marker >= 0:
                del pending[:marker]
            else:
                pending[:] = pending[-32:]
        elapsed = time.monotonic() - start
        status = terminal.poll()
        assert status == 0, f"player exited with {status}"
        print(f"{media.name}{' loop' if loop else ''}{' with input' if has_actions else ''}: "
              f"{elapsed:.2f}s / {duration:.2f}s; {frames} displayed frames; last position {last_second}s", flush=True)
        assert elapsed >= (duration - 0.15 if minimum_duration is None else minimum_duration), "player exited before queued media finished"
        if maximum_duration is not None:
            assert elapsed <= maximum_duration, "input did not interrupt EOF draining promptly"
        if expected_frames is not None:
            assert frames == expected_frames, f"lost decoded frames: {frames} / {expected_frames}"
        if final_red:
            assert is_red(last_pixels), "final red frame was not displayed"
        if loop:
            assert red_frames >= 48, "loop restarted before both final red segments played"
    finally:
        terminal.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=lambda value: Path(value).resolve(), required=True)
    parser.add_argument("--conpty", help="Windows: bundled Vivido conpty.dll (matching OpenConsole.exe beside it)")
    parser.add_argument("--media", type=lambda value: Path(value).resolve())
    args = parser.parse_args()
    if os.name == "nt" and not args.conpty:
        parser.error("Windows requires --conpty")
    with tempfile.TemporaryDirectory(prefix="kitim-eof-") as directory:
        for audio in (False, True):
            media = Path(directory) / ("audio.mp4" if audio else "silent.mp4")
            command = ["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=24:duration=4"]
            if audio:
                command += ["-f", "lavfi", "-i", "sine=frequency=440:duration=4"]
            command += ["-vf", "drawbox=color=red:t=fill:enable=gte(t\\,3)", "-c:v", "libx264", "-bf", "3", "-g", "96"]
            command += ["-c:a", "aac"] if audio else ["-an"]
            subprocess.run(command + [str(media)], check=True)
            play(args.binary, media, args.conpty, None if audio else 96, final_red=True)
            if audio:
                # With a two-second audio ring, demux EOF has already occurred by 3s.
                play(args.binary, media, args.conpty, final_red=True,
                     actions=[(3.0, b"g0\r")], minimum_duration=6.5)
                play(args.binary, media, args.conpty, actions=[(3.0, b"q")],
                     minimum_duration=2.9, maximum_duration=3.8)
            play(args.binary, media, args.conpty, loop=True,
                 actions=[(10.0, b"q")], minimum_duration=9.9, maximum_duration=10.8)
        if args.media:
            play(args.binary, args.media, args.conpty)


if __name__ == "__main__":
    main()
