#!/usr/bin/env python3
"""TCP proxy that adds a fixed one-way delay in each direction.

RTT = 2 x one-way delay. Byte order is preserved; there is no loss (userspace
TCP over loopback cannot lose packets in a meaningful way).
TRACE_FILE gets one line per chunk, "<monotonic-seconds> <u|d> <bytes>", stamped
when the proxy reads it (u = client to server, d = server to client). The
stamps use time.monotonic(), which is shared by all processes on one host, so
ui_bench.py can line them up with its own clock.
Each chunk read from a socket is stamped on arrival and written to the peer no
earlier than stamp + delay, so bursts keep their spacing instead of being
re-serialised.

--rate-down / --rate-up (kbit/s) cap one direction, default no cap. A capped
direction serialises chunks one after another at that rate, and the bytes
waiting for the link are bounded by --queue-kb (default 128). When the queue is
full the proxy stops reading its upstream socket, so TCP backpressure reaches
the sender and its buffers fill, as on a real slow link. Socket buffers on both
sides of the proxy hold more bytes on top of the queue. With a cap, the trace
stamps (and so ui_bench.py's wire bytes) still mark when the proxy read a chunk,
which can be up to a full queue ahead of its delivery.
"""
import argparse
import asyncio
import os
import socket
import time


TRACE_FD = None
DEFAULT_QUEUE_KB = 128
SOCKET_BUFFER = 64 * 1024


def trace(direction, size):
    # One unbuffered os.write per line, so a reader sees it mid-run.
    if TRACE_FD is not None and size:
        os.write(TRACE_FD, f"{time.monotonic():.6f} {direction} {size}\n".encode())


async def pump(reader, writer, delay, direction, link):
    queue = asyncio.Queue()
    # Bytes read but not yet written to the peer. Only bounded on a capped link.
    queued = 0
    room = asyncio.Event()
    # Small reads on a capped link keep the serialisation steps fine.
    read_size = 1 << 13 if link.rate else 1 << 16

    async def receive():
        nonlocal queued
        try:
            while True:
                while link.rate and queued >= link.queue_bytes:
                    room.clear()
                    await room.wait()
                data = await reader.read(read_size)
                trace(direction, len(data))
                queued += len(data)
                queue.put_nowait((time.monotonic(), data))
                if not data:
                    return
        except (ConnectionError, OSError):
            queue.put_nowait((time.monotonic(), b""))

    task = asyncio.create_task(receive())
    link_free = 0.0
    try:
        while True:
            stamp, data = await queue.get()
            start = max(stamp, link_free)
            link_free = start + len(data) / link.rate if link.rate else start
            wait = link_free + delay - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            if not data:
                break
            writer.write(data)
            await writer.drain()
            queued -= len(data)
            room.set()
    except (ConnectionError, OSError):
        pass
    finally:
        task.cancel()
        try:
            writer.close()
        except OSError:
            pass


def nodelay(writer, buffer=0):
    sock = writer.get_extra_info("socket")
    if sock is not None:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        if buffer:
            # Autotuned loopback buffers reach megabytes and would swallow the
            # queue bound.
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, buffer)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, buffer)


class Link:
    """One direction's bandwidth cap. rate is bytes per second, 0 for no cap."""

    def __init__(self, kbit, queue_kb):
        self.rate = kbit * 125
        self.queue_bytes = int(queue_kb * 1024)


async def serve(listen_port, upstream_port, one_way, down=None, up=None):
    down = down or Link(0, DEFAULT_QUEUE_KB)
    up = up or Link(0, DEFAULT_QUEUE_KB)
    # The stream buffer stops the transport reading at twice its limit, which
    # would hide 128 KB of the queue bound behind the default.
    capped = bool(down.rate or up.rate)
    limit = 1 << 14 if capped else 1 << 16
    sock_buffer = SOCKET_BUFFER if capped else 0

    async def handle(client_reader, client_writer):
        nodelay(client_writer, sock_buffer)
        # The kernel completes the local handshake instantly; charge the
        # link's handshake RTT before anything flows.
        await asyncio.sleep(2 * one_way)
        try:
            up_reader, up_writer = await asyncio.open_connection(
                "127.0.0.1", upstream_port, limit=limit)
        except OSError:
            client_writer.close()
            return
        nodelay(up_writer, sock_buffer)
        await asyncio.gather(
            pump(client_reader, up_writer, one_way, "u", up),
            pump(up_reader, client_writer, one_way, "d", down),
        )

    server = await asyncio.start_server(handle, "127.0.0.1", listen_port, limit=limit)
    async with server:
        await server.serve_forever()


async def selftest(rtt_ms, down, up):
    async def echo(reader, writer):
        while data := await reader.read(4096):
            writer.write(data)
            await writer.drain()
        writer.close()

    echo_server = await asyncio.start_server(echo, "127.0.0.1", 0)
    echo_port = echo_server.sockets[0].getsockname()[1]
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    proxy_port = probe.getsockname()[1]
    probe.close()
    proxy_task = asyncio.create_task(serve(proxy_port, echo_port, rtt_ms / 2000, down, up))
    await asyncio.sleep(0.1)
    reader, writer = await asyncio.open_connection("127.0.0.1", proxy_port)
    nodelay(writer)
    await asyncio.sleep(2 * rtt_ms / 1000 + 0.05)
    samples = []
    for _ in range(20):
        t0 = time.monotonic()
        writer.write(b"x")
        await reader.readexactly(1)
        samples.append((time.monotonic() - t0) * 1000)
    samples.sort()
    print(f"proxy rtt check (target {rtt_ms:g} ms): median {samples[len(samples) // 2]:.1f} ms, "
          f"min {samples[0]:.1f}, max {samples[-1]:.1f}")
    rates = [link.rate for link in (down, up) if link.rate]
    if rates:
        size = 100 * 1024
        t0 = time.monotonic()
        writer.write(b"x" * size)
        await reader.readexactly(size)
        took = (time.monotonic() - t0) * 1000
        # The echo comes back while the upload is still running, so the
        # slower direction sets the pace.
        print(f"proxy rate check: {size // 1024} KB echoed in {took:.0f} ms "
              f"(expect ~{rtt_ms + size / min(rates) * 1000:.0f} ms)")
    writer.close()
    proxy_task.cancel()
    echo_server.close()


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        usage="%(prog)s [--rate-down KBIT] [--rate-up KBIT] [--queue-kb KB] "
        "LISTEN_PORT UPSTREAM_PORT RTT_MS [TRACE_FILE]\n"
        "       %(prog)s [--rate-down KBIT] [--rate-up KBIT] --selftest RTT_MS",
    )
    parser.add_argument("--selftest", type=float, metavar="RTT_MS",
                        help="measure the added round trip (and the rate cap, if set) "
                             "through a throwaway echo server and exit")
    parser.add_argument("--rate-down", type=float, default=0, metavar="KBIT",
                        help="cap server to client at KBIT kbit/s (default: no cap)")
    parser.add_argument("--rate-up", type=float, default=0, metavar="KBIT",
                        help="cap client to server at KBIT kbit/s (default: no cap)")
    parser.add_argument("--queue-kb", type=float, default=DEFAULT_QUEUE_KB, metavar="KB",
                        help=f"bytes a capped direction holds before it stops reading "
                             f"(default {DEFAULT_QUEUE_KB})")
    parser.add_argument("listen_port", nargs="?", type=int)
    parser.add_argument("upstream_port", nargs="?", type=int)
    parser.add_argument("rtt_ms", nargs="?", type=float)
    parser.add_argument("trace_file", nargs="?")
    args = parser.parse_args()

    if min(args.rate_down, args.rate_up) < 0 or args.queue_kb <= 0:
        parser.error("--rate-down and --rate-up must not be negative, --queue-kb must be positive")
    down = Link(args.rate_down, args.queue_kb)
    up = Link(args.rate_up, args.queue_kb)

    if args.selftest is not None:
        asyncio.run(selftest(args.selftest, down, up))
        return
    if args.rtt_ms is None:
        parser.error("LISTEN_PORT, UPSTREAM_PORT and RTT_MS are required")
    global TRACE_FD
    if args.trace_file:
        TRACE_FD = os.open(args.trace_file, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    asyncio.run(serve(args.listen_port, args.upstream_port, args.rtt_ms / 2000, down, up))


if __name__ == "__main__":
    main()
