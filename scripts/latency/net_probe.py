#!/usr/bin/env python3
"""Diagnose the network link from this machine to a remote herdr machine.

HOST is an ssh destination such as andrej@jan-box. With no probe names, all
run.

  ssh   small-echo round trips and one bulk echo over `ssh -T HOST cat`
  ping  packet loss and RTT to HOST and to a public reference (bufferbloat)
  udp   datagram loss in both directions using back-to-back bursts

The remote side needs only python3 (stdlib) reachable over a non-interactive
ssh command. Use --json FILE to keep raw samples for comparing runs.
"""

import argparse
import json
import math
import os
import re
import select
import shlex
import socket
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

PROBES = ("ssh", "ping", "udp")

# Runs on both ends via `python3 -c`, so the receiver and sender are the same
# code locally and remotely.
HELPER_SRC = r"""
import json, os, select, socket, struct, sys, time

def recv(port, tag, bursts, size, lifetime):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 << 20)
    s.bind(("0.0.0.0", port))
    seen = set()
    dups = 0
    def take(d):
        nonlocal dups
        if len(d) < 12:
            return
        t, b, i = struct.unpack("!4sII", d[:12])
        if t != tag:
            return
        if (b, i) in seen:
            dups += 1
        seen.add((b, i))
    print("READY", flush=True)
    end = time.time() + lifetime
    while time.time() < end:
        r, _, _ = select.select([s, 0], [], [], 0.5)
        if s in r:
            take(s.recvfrom(4096)[0])
        if 0 in r and not os.read(0, 4096):
            break
    s.setblocking(False)
    while True:
        try:
            take(s.recvfrom(4096)[0])
        except BlockingIOError:
            break
    per_burst = [0] * bursts
    per_index = {}
    for b, i in seen:
        if b < bursts:
            per_burst[b] += 1
        per_index[i] = per_index.get(i, 0) + 1
    print(json.dumps({"received": len(seen), "duplicates": dups,
                      "per_burst": per_burst,
                      "per_index": [per_index.get(i, 0) for i in range(max(per_index, default=-1) + 1)]}))

def send(host, port, tag, bursts, burst_size, size, gap):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    packets = [[struct.pack("!4sII", tag, b, i).ljust(size, b"x") for i in range(burst_size)]
               for b in range(bursts)]
    sent = errors = 0
    for burst in packets:
        for p in burst:
            try:
                s.sendto(p, (host, port))
                sent += 1
            except OSError:
                errors += 1
        time.sleep(gap)
    print(json.dumps({"sent": sent, "send_errors": errors}))

mode = sys.argv[1]
if mode == "recv":
    port, bursts, size, lifetime = int(sys.argv[2]), int(sys.argv[4]), int(sys.argv[5]), float(sys.argv[6])
    recv(port, bytes.fromhex(sys.argv[3]), bursts, size, lifetime)
else:
    a = sys.argv
    send(a[2], int(a[3]), bytes.fromhex(a[4]), int(a[5]), int(a[6]), int(a[7]), float(a[8]))
"""


class SetupError(Exception):
    pass


def last_line(text):
    lines = text.strip().splitlines()
    return lines[-1] if lines else "(no output)"


def stats(xs):
    s = sorted(xs)
    return {
        "n": len(s),
        "min": s[0],
        "median": statistics.median(s),
        "p90": s[min(len(s) - 1, math.ceil(0.9 * len(s)) - 1)],
        "max": s[-1],
    }


def fmt_stats(st):
    return "min %.1f  median %.1f  p90 %.1f  max %.1f ms" % (
        st["min"], st["median"], st["p90"], st["max"])


class Link:
    def __init__(self, args):
        self.host = args.host
        self.ssh = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", *args.ssh_arg]
        self.remote_python = args.remote_python
        self.address = self._ssh_hostname(args)

    def _ssh_hostname(self, args):
        p = subprocess.run(["ssh", *args.ssh_arg, "-G", self.host],
                           capture_output=True, text=True)
        for line in p.stdout.splitlines():
            if line.startswith("hostname "):
                return line.split(None, 1)[1]
        raise SetupError("cannot resolve %s with `ssh -G`: %s" % (self.host, p.stderr.strip()))

    def remote_cmd(self, *argv):
        return [*self.ssh, self.host, shlex.join(argv)]

    def check_remote_python(self):
        p = subprocess.run(self.remote_cmd(self.remote_python, "-c", "print(1)"),
                           capture_output=True, text=True, timeout=30)
        if p.returncode != 0 or p.stdout.strip() != "1":
            raise SetupError("cannot run %s on %s (exit %d): %s" % (
                self.remote_python, self.host, p.returncode, p.stderr.strip() or p.stdout.strip()))


def read_exact(fd, n, timeout):
    """Read n bytes; None on timeout, ConnectionError if the stream ends."""
    got = 0
    while got < n:
        r, _, _ = select.select([fd], [], [], timeout)
        if not r:
            return None
        chunk = os.read(fd, n - got)
        if not chunk:
            raise ConnectionError("ssh stream closed")
        got += len(chunk)
    return got


def bulk_echo(fin, fout, payload, timeout):
    """Send payload while reading the echo, so neither pipe can deadlock."""
    os.set_blocking(fin, False)
    view = memoryview(payload)
    sent = got = 0
    start = time.perf_counter()
    while got < len(payload):
        r, w, _ = select.select([fout], [fin] if sent < len(payload) else [], [], timeout)
        if not r and not w:
            return None
        if w:
            sent += os.write(fin, view[sent:sent + 65536])
        if r:
            chunk = os.read(fout, 65536)
            if not chunk:
                raise ConnectionError("ssh stream closed")
            got += len(chunk)
    return (time.perf_counter() - start) * 1000


def probe_ssh(link, args):
    p = subprocess.Popen([*link.ssh, link.host, "cat"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
    fin, fout = p.stdin.fileno(), p.stdout.fileno()
    res = {"samples_ms": [], "timeouts": 0, "error": None, "bulk_bytes": args.bulk_bytes, "bulk_ms": None}
    try:
        os.write(fin, b"warm\n")
        try:
            warm = read_exact(fout, 5, 20)
        except ConnectionError:
            warm = None
            p.wait(timeout=5)
            raise SetupError("ssh to %s failed: %s" % (link.host, p.stderr.read().decode().strip()))
        if warm is None:
            raise SetupError("ssh to %s: no reply within 20 s" % link.host)
        msg = b"x" * 64 + b"\n"
        try:
            for _ in range(args.samples):
                t = time.perf_counter()
                os.write(fin, msg)
                if read_exact(fout, len(msg), args.read_timeout) is None:
                    res["timeouts"] += 1
                    break
                res["samples_ms"].append((time.perf_counter() - t) * 1000)
                time.sleep(args.spacing_ms / 1000)
            else:
                res["bulk_ms"] = bulk_echo(fin, fout, b"y" * args.bulk_bytes, args.read_timeout)
                if res["bulk_ms"] is None:
                    res["timeouts"] += 1
        except ConnectionError as e:
            res["error"] = str(e)
    finally:
        p.stdin.close()
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    if res["samples_ms"]:
        res["summary"] = stats(res["samples_ms"])
    return res


def report_ssh(res):
    print("== ssh: echo over `ssh -T HOST cat` ==")
    if res["samples_ms"]:
        print("  small echo (%d samples): %s" % (len(res["samples_ms"]), fmt_stats(res["summary"])))
    if res["bulk_ms"] is not None:
        print("  bulk echo %d bytes: %.0f ms" % (res["bulk_bytes"], res["bulk_ms"]))
    if res["timeouts"]:
        print("  TIMEOUT: a read stalled (no data for the read timeout)")
    if res["error"]:
        print("  ERROR: %s" % res["error"])


PING_LOSS = re.compile(r"(\d+) packets transmitted, (\d+) (?:packets )?received,.*?([\d.]+)% packet loss")
PING_RTT = re.compile(r"(?:round-trip|rtt) min/avg/max/(?:stddev|mdev) = ([\d.]+)/([\d.]+)/([\d.]+)/([\d.]+)")


def ping_one(target, args):
    cmd = ["ping", "-c", str(args.ping_count), "-i", str(args.ping_interval), target]
    res = {"target": target, "timed_out": False}
    try:
        p = subprocess.run(cmd, capture_output=True, text=True,
                           timeout=args.ping_count * args.ping_interval + 30)
    except subprocess.TimeoutExpired:
        res["timed_out"] = True
        return res
    out = p.stdout
    m = PING_LOSS.search(out)
    if not m:
        raise SetupError("cannot parse ping output for %s: %s" % (target, (out + p.stderr).strip()))
    res["sent"], res["received"], res["loss_pct"] = int(m[1]), int(m[2]), float(m[3])
    res["rtts_ms"] = [float(x) for x in re.findall(r"time=([\d.]+) ms", out)]
    m = PING_RTT.search(out)
    if m:
        res.update(zip(("min_ms", "avg_ms", "max_ms", "stddev_ms"), map(float, m.groups())))
    return res


def probe_ping(link, args):
    return [ping_one(link.address, args), ping_one(args.ref_host, args)]


def report_ping(res):
    print("== ping ==")
    for r in res:
        if r["timed_out"]:
            print("  %-16s TIMEOUT" % r["target"])
        elif "min_ms" not in r:
            print("  %-16s loss %.1f%% (%d/%d), no replies" % (r["target"], r["loss_pct"], r["received"], r["sent"]))
        else:
            print("  %-16s loss %.1f%% (%d/%d)  min %.1f  avg %.1f  max %.1f  stddev %.1f ms" % (
                r["target"], r["loss_pct"], r["received"], r["sent"],
                r["min_ms"], r["avg_ms"], r["max_ms"], r["stddev_ms"]))
    print("  (a big gap between avg and min means queueing delay, i.e. bufferbloat)")


def udp_direction(rx_cmd, tx_cmd, args):
    """rx_cmd/tx_cmd are the command lists that start the receiver/sender helpers."""
    rx = subprocess.Popen(rx_cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.PIPE)
    try:
        ready = b""
        deadline = time.time() + 30
        while not ready.endswith(b"\n"):
            r, _, _ = select.select([rx.stdout], [], [], max(0.0, deadline - time.time()))
            byte = os.read(rx.stdout.fileno(), 1) if r else b""
            if not byte:
                rx.kill()
                raise SetupError("udp receiver did not start: %s" % last_line(rx.stderr.read().decode()))
            ready += byte
        gap_s = args.burst_gap_ms / 1000
        tx = subprocess.run(tx_cmd, capture_output=True, text=True,
                            timeout=args.bursts * (gap_s + 0.1) + 30)
        if tx.returncode != 0:
            raise SetupError("udp sender failed: %s" % last_line(tx.stderr))
        sent = json.loads(tx.stdout.strip().splitlines()[-1])
        time.sleep(1.0)
        rx.stdin.close()
        out, err = rx.communicate(timeout=15)
        if rx.returncode != 0:
            raise SetupError("udp receiver failed: %s" % last_line(err.decode()))
        got = json.loads(out.decode().strip().splitlines()[-1])
    finally:
        if rx.poll() is None:
            rx.kill()
            rx.wait()
    res = {**sent, **got}
    res["loss_pct"] = 100 * (sent["sent"] - got["received"]) / sent["sent"] if sent["sent"] else None
    res["bursts_with_loss"] = sum(1 for c in got["per_burst"] if c < args.burst_size)
    return res


def probe_udp(link, args):
    link.check_remote_python()
    tag = os.urandom(4).hex()
    lifetime = args.bursts * (args.burst_gap_ms / 1000 + 0.1) + 60

    def local(*argv):
        return [sys.executable, "-c", HELPER_SRC, *map(str, argv)]

    def remote(*argv):
        return link.remote_cmd(link.remote_python, "-c", HELPER_SRC, *map(str, argv))

    recv_args = ("recv", args.port, tag, args.bursts, args.datagram_size, lifetime)
    send_tail = (tag, args.bursts, args.burst_size, args.datagram_size, args.burst_gap_ms / 1000)

    local_addr = args.local_addr or local_address_for(link.address)
    down = udp_direction(local(*recv_args), remote("send", local_addr, args.port, *send_tail), args)
    up = udp_direction(remote(*recv_args), local("send", link.address, args.port, *send_tail), args)
    return {
        "local_addr": local_addr,
        "remote_addr": link.address,
        "port": args.port,
        "bursts": args.bursts,
        "burst_size": args.burst_size,
        "datagram_size": args.datagram_size,
        "burst_gap_ms": args.burst_gap_ms,
        "down_remote_to_local": down,
        "up_local_to_remote": up,
    }


def local_address_for(remote_host):
    try:
        ip = socket.getaddrinfo(remote_host, None, socket.AF_INET)[0][4][0]
    except socket.gaierror as e:
        raise SetupError("cannot resolve %s: %s" % (remote_host, e))
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.connect((ip, 9))
        return s.getsockname()[0]


def report_udp(res):
    print("== udp: %d bursts x %d datagrams x %d bytes, %d ms apart, %s <-> %s:%d ==" % (
        res["bursts"], res["burst_size"], res["datagram_size"], res["burst_gap_ms"],
        res["local_addr"], res["remote_addr"], res["port"]))
    for label, key in (("down (remote -> local)", "down_remote_to_local"),
                       ("up   (local -> remote)", "up_local_to_remote")):
        r = res[key]
        line = "  %s: sent %d  received %d  loss %s" % (
            label, r["sent"], r["received"],
            "n/a" if r["loss_pct"] is None else "%.2f%%" % r["loss_pct"])
        line += "  bursts with loss %d/%d" % (r["bursts_with_loss"], res["bursts"])
        if r["send_errors"]:
            line += "  send errors %d" % r["send_errors"]
        if r["duplicates"]:
            line += "  duplicates %d" % r["duplicates"]
        print(line)


RUNNERS = {
    "ssh": (probe_ssh, report_ssh),
    "ping": (probe_ping, report_ping),
    "udp": (probe_udp, report_udp),
}


def parse_args():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 epilog=__doc__.split("\n\n", 1)[1],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("host", metavar="HOST", help="ssh destination, e.g. andrej@jan-box")
    ap.add_argument("probes", nargs="*", choices=PROBES, metavar="PROBE",
                    help="ssh, ping, udp (default: all)")
    ap.add_argument("--json", metavar="FILE", help="write raw samples and summaries as JSON")
    ap.add_argument("--ssh-arg", action="append", default=[], metavar="ARG",
                    help="extra ssh argument, repeatable (e.g. --ssh-arg=-oIPQoS=none)")
    ap.add_argument("--remote-python", default="python3", metavar="PATH")
    g = ap.add_argument_group("ssh probe")
    g.add_argument("--samples", type=int, default=30)
    g.add_argument("--spacing-ms", type=float, default=50)
    g.add_argument("--bulk-bytes", type=int, default=200000)
    g.add_argument("--read-timeout", type=float, default=5, help="seconds without data before a read counts as a timeout")
    g = ap.add_argument_group("ping probe")
    g.add_argument("--ping-count", type=int, default=50)
    g.add_argument("--ping-interval", type=float, default=0.2)
    g.add_argument("--ref-host", default="1.1.1.1", help="bufferbloat reference target")
    g = ap.add_argument_group("udp probe")
    g.add_argument("--port", type=int, default=5999)
    g.add_argument("--bursts", type=int, default=20)
    g.add_argument("--burst-size", type=int, default=50, help="datagrams per back-to-back burst")
    g.add_argument("--datagram-size", type=int, default=1200)
    g.add_argument("--burst-gap-ms", type=float, default=100,
                   help="pause between bursts; sets the average rate (below ~100 ms the probe can congest a slow uplink itself)")
    g.add_argument("--local-addr", help="this machine's address as reachable from the remote (default: route to HOST)")
    return ap.parse_args()


def main():
    args = parse_args()
    if args.datagram_size < 12:
        sys.exit("net_probe: --datagram-size must be at least 12")
    selected = args.probes or PROBES
    out = {"host": args.host, "started": datetime.now(timezone.utc).isoformat(),
           "argv": sys.argv[1:], "probes": {}}
    try:
        link = Link(args)
        for name in PROBES:
            if name not in selected:
                continue
            run, report = RUNNERS[name]
            out["probes"][name] = run(link, args)
            report(out["probes"][name])
            print()
    except SetupError as e:
        sys.exit("net_probe: error: %s" % e)
    except KeyboardInterrupt:
        sys.exit(130)
    if args.json:
        with open(args.json, "w") as f:
            json.dump(out, f, indent=2)
        print("wrote %s" % args.json)


if __name__ == "__main__":
    main()
