#!/usr/bin/env python3
"""Run the real `mobius-searcher` binary inside a pseudo-terminal of a given size,
feed it keys, and reconstruct the screen with a VT100 emulator (pyte).

This exercises the actual terminal path (raw mode, alternate screen, escape
sequences, resize handling) — not the off-screen test renderer.

usage: pty_render.py WIDTH HEIGHT [--keys KEYS] [--resize WxH] -- <command...>
requires: pip install pyte
"""
import codecs
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

import pyte


def set_size(fd, w, h):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", h, w, 0, 0))


DECODER = codecs.getincrementaldecoder("utf-8")("replace")


def pump(fd, stream, secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            stream.feed(DECODER.decode(data))
    return True


def main():
    args = sys.argv[1:]
    sep = args.index("--")
    opts, cmd = args[:sep], args[sep + 1 :]
    w, h = int(opts[0]), int(opts[1])
    keys = ""
    resize = None
    if "--keys" in opts:
        keys = opts[opts.index("--keys") + 1]
    if "--resize" in opts:
        rw, rh = opts[opts.index("--resize") + 1].split("x")
        resize = (int(rw), int(rh))

    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLORTERM"] = "truecolor"
        os.environ.setdefault("LANG", "en_US.UTF-8")
        os.execvp(cmd[0], cmd)
    set_size(fd, w, h)
    screen = pyte.Screen(w, h)
    stream = pyte.Stream(screen)
    pump(fd, stream, 2.5)
    for k in keys:
        os.write(fd, k.encode())
        pump(fd, stream, 0.4)
    if resize:
        set_size(fd, *resize)
        os.kill(pid, signal.SIGWINCH)
        screen.resize(resize[1], resize[0])
        pump(fd, stream, 1.5)
    print("\n".join(line.rstrip() for line in screen.display))
    os.write(fd, b"q")
    pump(fd, stream, 1.0)
    try:
        _, status = os.waitpid(pid, 0)
        code = os.waitstatus_to_exitcode(status)
    except ChildProcessError:
        code = "?"
    print(f"[exit {code}]", file=sys.stderr)


if __name__ == "__main__":
    main()
