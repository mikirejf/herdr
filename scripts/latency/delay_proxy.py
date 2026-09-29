#!/usr/bin/env python3
"""TCP proxy that adds a fixed one-way delay in each direction.

RTT = 2 x one-way delay. Byte order is preserved; there is no bandwidth cap and
no loss (userspace TCP over loopback cannot lose packets in a meaningful way).
TRACE_FILE gets one line per chunk, "<monotonic-seconds> <u|d> <bytes>", stamped
when the proxy reads it (u = client to server, d = server to client). The
stamps use time.monotonic(), which is shared by all processes on one host, so
ui_bench.py can line them up with its own clock.
Each chunk read from a socket is stamped on arrival and written to the peer no
earlier than stamp + delay, so bursts keep their spacing instead of being
re-serialised.
"""
import argparse
import asyncio
import os
import socket
import time


TRACE_FD = None


def trace(direction, size):
    # One unbuffered os.write per line, so a reader sees it mid-run.
    if TRACE_FD is not None and size:
        os.write(TRACE_FD, f"{time.monotonic():.6f} {direction} {size}\n".encode())


async def pump(reader, writer, delay, direction):
    queue = asyncio.Queue()

    async def receive():
        try:
            while True:
                data = await reader.read(1 << 16)
                trace(direction, len(data))
                queue.put_nowait((time.monotonic(), data))
                if not data:
                    return
        except (ConnectionError, OSError):
            queue.put_nowait((time.monotonic(), b""))

    task = asyncio.create_task(receive())
    try:
        while True:
            stamp, data = await queue.get()
            wait = stamp + delay - time.monotonic()
            if wait > 0:
                await asyncio.sleep(wait)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except (ConnectionError, OSError):
        pass
    finally:
        task.cancel()
        try:
            writer.close()
        except OSError:
            pass


def nodelay(writer):
    sock = writer.get_extra_info("socket")
    if sock is not None:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)


async def serve(listen_port, upstream_port, one_way):
    async def handle(client_reader, client_writer):
        nodelay(client_writer)
        # The kernel completes the local handshake instantly; charge the
        # link's handshake RTT before anything flows.
        await asyncio.sleep(2 * one_way)
        try:
            up_reader, up_writer = await asyncio.open_connection("127.0.0.1", upstream_port)
        except OSError:
            client_writer.close()
            return
        nodelay(up_writer)
        await asyncio.gather(
            pump(client_reader, up_writer, one_way, "u"),
            pump(up_reader, client_writer, one_way, "d"),
        )

    server = await asyncio.start_server(handle, "127.0.0.1", listen_port)
    async with server:
        await server.serve_forever()


async def selftest(rtt_ms):
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
    proxy_task = asyncio.create_task(serve(proxy_port, echo_port, rtt_ms / 2000))
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
    writer.close()
    proxy_task.cancel()
    echo_server.close()


def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        usage="%(prog)s LISTEN_PORT UPSTREAM_PORT RTT_MS [TRACE_FILE]\n"
        "       %(prog)s --selftest RTT_MS",
    )
    parser.add_argument("--selftest", type=float, metavar="RTT_MS",
                        help="measure the added round trip through a throwaway echo server and exit")
    parser.add_argument("listen_port", nargs="?", type=int)
    parser.add_argument("upstream_port", nargs="?", type=int)
    parser.add_argument("rtt_ms", nargs="?", type=float)
    parser.add_argument("trace_file", nargs="?")
    args = parser.parse_args()

    if args.selftest is not None:
        asyncio.run(selftest(args.selftest))
        return
    if args.rtt_ms is None:
        parser.error("LISTEN_PORT, UPSTREAM_PORT and RTT_MS are required")
    global TRACE_FD
    if args.trace_file:
        TRACE_FD = os.open(args.trace_file, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    asyncio.run(serve(args.listen_port, args.upstream_port, args.rtt_ms / 2000))


if __name__ == "__main__":
    main()
