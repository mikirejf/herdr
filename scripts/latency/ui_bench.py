#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["pyte>=0.8.2"]
# ///
"""Time herdr UI actions by driving a real client in a pty.

The client command goes after `--`. Examples:

  ui_bench.py -n 5 --pane-cols 60,150 -- herdr --session probe
  ui_bench.py --remote-ws '· box' --remote-marker MARK1 \\
      --local-ws '· local' --local-marker MARK2 -- mosh me@box -- herdr

The client runs in a pty (default 200x50) and is rendered with pyte. Inherited
HERDR_* variables are removed from its environment and TERM=xterm-256color.
Clicks are SGR mouse events. Scenarios run only when configured:

  workspace switch   --remote-ws/--remote-marker/--local-ws/--local-marker
                     (all four). Clicks the sidebar labels (found in the first
                     30 columns) alternately. Remote is visible when its marker
                     is on screen; local when its marker is on screen and the
                     remote marker is not.
  pane focus         --pane-cols A,B. Clicks column A and B alternately at
                     mid-height of the current screen. Visible when any cell of
                     the screen changes, so nothing else may be animating.
                     If a workspace switch ran, the remote workspace is current.

"visible" is when the chunk of pty output that made the condition true arrived,
"settled" is the last pty output before 0.8 s of quiet, both in ms from the
click. With --trace FILE (delay_proxy.py output, only useful when the client's
traffic goes through the proxy) the wire bytes up/down and the number of
up->down turnarounds in that window are added, and "wire ms": when the last
downstream chunk at or before "visible" passed the proxy. "visible" minus "wire
ms" is time spent after the bytes left the network. Timeouts are reported, not
averaged in, and make the exit status 3. A missing sidebar label or a client
that exits prints the screen and exits 1.
"""
import argparse
import datetime
import fcntl
import json
import math
import os
import pty
import queue
import shlex
import signal
import statistics
import struct
import sys
import termios
import threading
import time

import pyte

SIDEBAR_COLS = 30
QUIET = 0.8


class ClientExited(Exception):
    pass


class Screen(pyte.Screen):
    # pyte would otherwise call a write_process_input hook that we don't
    # provide when the client asks for status/attributes, and it rejects the
    # private flag on SGR sequences.
    def report_device_status(self, mode, **kw):
        pass

    def report_device_attributes(self, *a, **kw):
        pass

    def select_graphic_rendition(self, *a, **kw):
        kw.pop("private", None)
        super().select_graphic_rendition(*a)


class Client:
    def __init__(self, argv, cols, rows, trace_path):
        env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
        env["TERM"] = "xterm-256color"
        env.setdefault("LANG", "en_US.UTF-8")
        self.cols, self.rows = cols, rows
        self.trace_path = trace_path
        self.screen = Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.nbytes = 0
        self.last_read = 0.0
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                os.execvpe(argv[0], argv, env)
            except OSError as err:
                os.write(2, f"ui_bench: cannot run {argv[0]}: {err}\n".encode())
                os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        # pyte needs ~100 ms for a full colored 300x80 frame. A reader that waited for it would
        # stamp the next chunks late and block the client's writes, so a thread stamps every
        # chunk as it arrives. The pty hands over ~1 KB per read and the thread needs the GIL
        # after each one, so pyte must give it up far sooner than the default 5 ms.
        sys.setswitchinterval(0.0001)
        self.chunks = queue.Queue()
        threading.Thread(target=self.read_pty, daemon=True).start()

    def read_pty(self):
        while True:
            try:
                data = os.read(self.fd, 1 << 20)
            except OSError:
                data = b""
            self.chunks.put((time.monotonic(), data))
            if not data:
                return

    def close(self):
        # The client ignores SIGTERM while attached, so escalate.
        for sig in (signal.SIGHUP, signal.SIGKILL):
            try:
                os.kill(self.pid, sig)
            except ProcessLookupError:
                break
            for _ in range(20):
                try:
                    if os.waitpid(self.pid, os.WNOHANG)[0]:
                        return
                except ChildProcessError:
                    return
                time.sleep(0.1)

    def pump(self, timeout):
        """Feed one chunk to the screen. `last_read` is when that chunk arrived."""
        try:
            self.last_read, data = self.chunks.get(timeout=timeout)
        except queue.Empty:
            return 0
        if not data:
            raise ClientExited
        self.stream.feed(data)
        self.nbytes += len(data)
        return len(data)

    def settle(self, quiet, maxt=20):
        start = last = time.monotonic()
        while time.monotonic() - start < maxt:
            if self.pump(0.05):
                last = self.last_read
            elif time.monotonic() - last > quiet:
                return

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display).rstrip()

    def snapshot(self):
        blank = self.screen.default_char
        return [
            {x: c for x, c in self.screen.buffer[y].items() if c != blank}
            for y in range(self.rows)
        ]

    def wait_for_text(self, needle, timeout):
        end = time.monotonic() + timeout
        while needle not in self.text():
            if time.monotonic() > end:
                sys.exit(f"ui_bench: {needle!r} never appeared\n{self.text()}")
            self.pump(0.1)

    def find(self, label, timeout=20):
        end = time.monotonic() + timeout
        while True:
            for y, line in enumerate(self.screen.display):
                x = line.find(label)
                if 0 <= x < SIDEBAR_COLS:
                    return x + len(label) // 2 + 1, y + 1
            if time.monotonic() > end:
                sys.exit(f"ui_bench: label not found: {label!r}\n{self.text()}")
            self.pump(0.1)

    def click(self, x, y):
        os.write(self.fd, f"\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m".encode())

    def wire(self, t0, t1, hit):
        """Wire bytes and up->down turnarounds through the proxy in [t0, t1], and the stamp of
        the last downstream chunk at or before `hit`."""
        up = down = turns = 0
        last = None
        last_down = t0
        with open(self.trace_path) as f:
            for line in f:
                stamp, direction, size = line.split()
                if not t0 <= float(stamp) <= t1:
                    continue
                if direction == "u":
                    up += int(size)
                else:
                    down += int(size)
                    if float(stamp) <= hit:
                        last_down = float(stamp)
                    if last == "u":
                        turns += 1
                last = direction
        return up, down, turns, last_down

    def idle(self, seconds):
        end = time.monotonic() + seconds
        while (left := end - time.monotonic()) > 0:
            self.pump(min(left, 0.1))

    def measure(self, label, done, action, timeout, idle=0):
        self.idle(idle)
        self.settle(0.5)
        before = self.snapshot()
        b0 = self.nbytes
        t0 = time.monotonic()
        action()
        hit = last = None
        while time.monotonic() - t0 < timeout:
            if self.pump(0.005):
                last = self.last_read
                if hit is None and done(before):
                    hit = last
            elif hit is not None and time.monotonic() - last > QUIET:
                break
        sample = {"timeout": hit is None, "visible": None, "settled": None,
                  "pty": self.nbytes - b0, "up": None, "down": None, "turns": None,
                  "wire": None}
        if hit is None:
            print(f"  {label}: TIMEOUT after {timeout:g} s", file=sys.stderr, flush=True)
            return sample
        sample["visible"] = (hit - t0) * 1000
        sample["settled"] = (last - t0) * 1000
        if self.trace_path:
            sample["up"], sample["down"], sample["turns"], last_down = self.wire(t0, last, hit)
            sample["wire"] = (last_down - t0) * 1000
        print(f"  {label}: visible {sample['visible']:.0f} ms", file=sys.stderr, flush=True)
        return sample


def stats(values):
    v = sorted(values)
    if not v:
        return None
    return {"min": v[0], "median": statistics.median(v),
            "p90": v[max(0, math.ceil(0.9 * len(v)) - 1)], "max": v[-1]}


def summarize(samples):
    ok = [s for s in samples if not s["timeout"]]
    out = {"n": len(samples), "timeouts": len(samples) - len(ok),
           "visible": stats(s["visible"] for s in ok),
           "settled": stats(s["settled"] for s in ok)}
    for key in ("pty", "up", "down", "turns", "wire"):
        values = [s[key] for s in ok if s[key] is not None]
        out[key] = statistics.median(values) if values else None
    return out


def print_table(results, traced):
    num = lambda v, w: f"{'-':>{w}}" if v is None else f"{v:>{w}.0f}"
    head = f"{'scenario':<24}{'ok/n':>6} | {'visible ms':^27} | {'settled ms':^27} | {'pty B':>8}"
    sub = f"{'':<24}{'':>6} | {'min':>6}{'med':>7}{'p90':>7}{'max':>7} | {'min':>6}{'med':>7}{'p90':>7}{'max':>7} | {'(median)':>8}"
    if traced:
        head += f" | {'wire B (median)':^19} | {'turns':>5} | {'wire ms':>7}"
        sub += f" | {'up':>9}{'down':>10} | {'':>5} | {'(median)':>7}"
    print(head)
    print(sub)
    for name, samples in results.items():
        s = summarize(samples)
        row = f"{name:<24}{str(s['n'] - s['timeouts']) + '/' + str(s['n']):>6} |"
        for key in ("visible", "settled"):
            st = s[key]
            cells = [num(st and st[k], 7 if k != "min" else 6) for k in ("min", "median", "p90", "max")]
            row += " " + "".join(cells) + " |"
        row += " " + num(s["pty"], 8)
        if traced:
            row += f" | {num(s['up'], 9)}{num(s['down'], 10)} | {num(s['turns'], 5)} | {num(s['wire'], 7)}"
        if s["timeouts"]:
            row += f"   TIMEOUT x{s['timeouts']}"
        print(row)


def run_scenarios(client, args):
    results = {}
    timeout = args.timeout

    def record(name, done, action):
        results.setdefault(name, []).append(
            client.measure(name, done, action, timeout, args.idle))

    if args.remote_ws:
        remote_xy = client.find(args.remote_ws)
        local_xy = client.find(args.local_ws)
        at_remote = lambda before: args.remote_marker in client.text()
        at_local = lambda before: (args.local_marker in client.text()
                                   and args.remote_marker not in client.text())
        to_remote = lambda: client.click(*remote_xy)
        to_local = lambda: client.click(*local_xy)

        record("first switch -> remote", at_remote, to_remote)
        client.measure("warm-up -> local", at_local, to_local, timeout)
        for _ in range(args.n):
            record("switch -> remote", at_remote, to_remote)
            record("switch -> local", at_local, to_local)
        client.measure("warm-up -> remote", at_remote, to_remote, timeout)

    if args.pane_cols:
        y = client.rows // 2
        first, second = (lambda: client.click(args.pane_cols[0], y),
                         lambda: client.click(args.pane_cols[1], y))
        changed = lambda before: client.snapshot() != before
        # Start from a known pane so every measured click really moves focus.
        # The pane may already be focused, so don't wait for a change.
        first()
        client.settle(QUIET)
        for i in range(args.n):
            record("pane focus", changed, second if i % 2 == 0 else first)
    return results


def parse_args():
    argv = sys.argv[1:]
    client = []
    if "--" in argv:
        i = argv.index("--")
        argv, client = argv[:i], argv[i + 1:]
    p = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter,
        usage="%(prog)s [options] -- CLIENT_COMMAND [ARGS...]")
    p.add_argument("--remote-ws", metavar="LABEL", help="sidebar label of the remote workspace")
    p.add_argument("--remote-marker", metavar="TEXT", help="text shown only in the remote workspace")
    p.add_argument("--local-ws", metavar="LABEL", help="sidebar label of the local workspace")
    p.add_argument("--local-marker", metavar="TEXT", help="text shown only in the local workspace")
    p.add_argument("--pane-cols", metavar="A,B", help="two screen columns (1-based) to click alternately")
    p.add_argument("-n", type=int, default=5, help="measured repetitions per scenario (default 5)")
    p.add_argument("--timeout", type=float, default=30, metavar="SEC", help="per action (default 30)")
    p.add_argument("--idle", type=float, default=0, metavar="SEC",
                   help="wait this long before each measured action, so the link goes idle "
                        "the way it does between real clicks (default 0)")
    p.add_argument("--cols", type=int, default=200)
    p.add_argument("--rows", type=int, default=50)
    p.add_argument("--wait-text", metavar="TEXT", help="before starting, wait up to 60 s for TEXT on screen")
    p.add_argument("--trace", metavar="FILE", help="delay_proxy.py trace file")
    p.add_argument("--json", metavar="FILE", help="also write raw samples and the summary here")
    p.add_argument("--label", help="free text stored in the output (default: the client command)")
    args = p.parse_args(argv)

    if not client:
        p.error("give the client command after --")
    ws = [args.remote_ws, args.remote_marker, args.local_ws, args.local_marker]
    if any(ws) and not all(ws):
        p.error("workspace switch needs --remote-ws, --remote-marker, --local-ws and --local-marker")
    if args.pane_cols:
        try:
            args.pane_cols = [int(c) for c in args.pane_cols.split(",")]
            assert len(args.pane_cols) == 2
        except (ValueError, AssertionError):
            p.error("--pane-cols wants two integers like 60,150")
    if not args.remote_ws and not args.pane_cols:
        p.error("configure a scenario: workspace switch flags and/or --pane-cols")
    if args.trace and not os.path.isfile(args.trace):
        p.error(f"--trace file not found: {args.trace}")
    return args, client


def main():
    args, argv = parse_args()
    label = args.label or shlex.join(argv)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    client = Client(argv, args.cols, args.rows, args.trace)
    try:
        client.settle(1.5, 15)
        if args.wait_text:
            client.wait_for_text(args.wait_text, 60)
        results = run_scenarios(client, args)
    except ClientExited:
        sys.exit(f"ui_bench: client exited\n{client.text()}")
    finally:
        client.close()

    print(f"client: {label}\n{args.cols}x{args.rows}  n={args.n}  "
          f"times in ms from the click; bytes per action")
    print_table(results, bool(args.trace))
    if args.json:
        with open(args.json, "w") as f:
            json.dump({
                "label": label, "client": argv, "cols": args.cols, "rows": args.rows,
                "n": args.n, "timeout_s": args.timeout, "traced": bool(args.trace),
                "date": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
                "samples": results,
                "summary": {name: summarize(s) for name, s in results.items()},
            }, f, indent=2)
    if any(s["timeout"] for samples in results.values() for s in samples):
        sys.exit(3)


if __name__ == "__main__":
    main()
