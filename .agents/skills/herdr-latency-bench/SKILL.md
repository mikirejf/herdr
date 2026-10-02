---
name: herdr-latency-bench
description: Measure herdr UI latency and remote-link health with the tools in scripts/latency/. Use before and after any change to remote attach, focus, surface, or snapshot code, when a remote machine feels slow, or when comparing two herdr builds.
---

# Herdr latency bench

Reuse these tools. Extend one when it lacks a scenario; write a new script only for a question none of them can ask. Each tool's `--help` is the usage reference.

| Question | Tool |
| --- | --- |
| Did my change make the remote UI faster, in round trips and bytes? | `scripts/latency/rig.sh run [RTT_MS=66] [N=5]` |
| How fast is the real remote machine right now? | `scripts/latency/ui_bench.py ... -- <client command>` |
| Is the slowness the network (loss, bufferbloat) and not herdr? | `scripts/latency/net_probe.py andrej@jan-box` |
| Does typing echo stay fast behind a burst of screen data on a slow link? | `RATE_KBIT=1000 scripts/latency/rig.sh run 66 20` (see Capping bandwidth) |

## Comparing builds with the rig

The rig is a local sshd behind `delay_proxy.py`, a fixed-delay TCP proxy, with fake client and remote HOMEs. It never touches the real server, `~/.config/herdr` or `~/.ssh`.

1. Build the candidate: `cargo build --release`. The rig uses `target/release/herdr` for the client and the local server unless `HERDR_BIN` is set.
2. Run a baseline, then the candidate, the same way. Save both with `HRIG_JSON=/tmp/<name>.json`, for example `HERDR_BIN=<baseline build> HRIG_JSON=/tmp/base.json scripts/latency/rig.sh run 66 10`.
3. Compare visible and settled medians, the `turns` column (round trips per action), and wire bytes. At a fixed delay, `turns` and bytes are the numbers a change actually moves. The ms figures follow from them.

Set `HERDR_REMOTE_BIN` to a fork build when the change is on the remote side. By default the remote runs the `herdr` on PATH.

Baseline at 66 ms, fork build on both ends (`c8cd5267`), medians with p90 in brackets:

| Action | Loss | Visible | Settled | Turns | Bytes down |
| --- | --- | --- | --- | --- | --- |
| Switch to remote | none | ~22 ms (25) | ~83 ms (86) | 1 | ~90 B |
| Switch to remote | 1% down | ~20 ms (24) | ~82 ms (85) | 1 | ~90 B |
| Switch to local | 1% down | ~21 ms (25) | ~33 ms (37) | 0 | 0 |
| Pane focus | none | ~1 ms | ~72 ms (72) | 1 | ~0.3 KB |
| Pane focus | 1% down | ~1 ms | ~72 ms (99) | 1 | ~0.3 KB |

Besides the switch and pane focus, `rig.sh run` measures two more scenarios. The wheel scenario guards a remote-side fix, so set `HERDR_REMOTE_BIN` to the build under test as well as `HERDR_BIN`. The echo scenario guards client-only changes (prefetch pacing and the idle gate), so `HERDR_BIN` is enough.

- `wheel up` / `wheel down` (`--scroll-col`): one SGR wheel event on the right remote pane, which has 5000 rows of random hex scrollback (every row differs, so a full delta is large). Guards `d824ac89`: a remote wheel step sends a small scroll patch, not a full surface delta. At 66 ms, `Bytes down` is ~70 B per step with the fix and ~450-600 B before it.
- `echo in prefetch` (`--echo-text`, `--echo-delay`): the remote workspace has 8 unvisited tabs of full colored screens. Each action goes local, resizes the client pty by one column, clicks the remote (the new size makes the client prefetch all 8 tab screens again), waits `--echo-delay` ms (350) and types a token into a remote shell prompt. Time is from keypress to the token on screen. Guards `1665a120` (prefetch asks for one tab at a time, so an echo is not queued behind every full screen) and the idle gate (no new tab screen is asked for until the user has stopped typing for 1 s). With the idle gate the echo happens before any prefetch can start, so on a build that has it this scenario measures avoided interference, not echo during prefetch. Expect about one RTT. The rig link has no bandwidth limit, so the old all-at-once prefetch finishes about 100 ms after the click and a 350 ms delay misses it; run `ui_bench.py ... --echo-delay 100` against a rig set up with `rig.sh setup` to hit that burst (before the fix ~77 ms median, p90 95, max 158; with it ~70 ms, p90 73). Also watch `Bytes down` for how much prefetch traffic shares the window. Check that prefetch really overlapped the typing with `--trace` (one ~36 B up and one 3-8 KB down per tab, spread over the first second).

The rig's switch alternates between the same remote and local workspace, so the remote is always warm: its surface streams in the background and the switch presents it without waiting. The remaining turn is the foreground handshake and window title, which arrive after the frame. `first switch -> remote` is the cold path, one round trip (~100 ms).

The goal is local speed: ~22 ms to switch to a local workspace, with no link traffic. Loss moves the tail, not the median, so judge loss runs by p90 and max with `N` of 30 or more.

## Adding packet loss

The real jan-box link loses ~1%, mostly server to client, and TCP turns one lost segment into 250 ms to several seconds of recovery. The proxy can't drop segments, so `lo_netem.sh` adds delay and loss in the kernel (macOS dummynet) on the rig's sshd port only, and clamps segments to Tailscale's size so a screen update spans as many packets as on the real link.

1. `sudo scripts/latency/lo_netem.sh on 66 1`: RTT, loss down %, optional loss up %.
2. `scripts/latency/rig.sh run 66 30`. The rig prints a `shaping:` line and a `shaped path check`; the check must read ~66 ms and `mss 1216`, or the shaping is not active.
3. `rig.sh stop` if you used `setup`, then `sudo scripts/latency/lo_netem.sh off`.

Dummynet loss is random per packet; the real link drops back-to-back segments, so the real tail is worse. `lo_netem.sh status` shows whether shaping is on.

## Capping bandwidth

Without a cap, bytes never pile up on the rig link. `RATE_KBIT=<kbit/s>` makes `delay_proxy.py` serialise each direction at that rate with a bounded queue (`QUEUE_KB`, default 128). A full queue stops the proxy reading, so TCP backpressure fills sshd's buffers as on a real slow link. `rig.sh` prints `shaping: proxy rate cap ...` and adds the `echo under bulk` scenario to `run`:

```
HRIG_DIR=/tmp/hrbw HERDR_BIN=$PWD/target/release/herdr HERDR_REMOTE_BIN=$PWD/target/release/herdr \
  RATE_KBIT=1000 scripts/latency/rig.sh run 66 20
```

- `echo idle` (`--bulk-echo-text`, needs `--pane-cols`): N tokens typed into the left remote pane with the link quiet, 1-2 s apart. The baseline for the next row.
- `echo under bulk`: the same, while the right remote pane runs an endless loop of colored random hex. Time is from keypress to the token on screen, so it includes any wait behind queued screen data. A line after the table gives bytes down and the average rate for the whole scenario. A sample that does not show up in `--timeout` (30 s) counts as TIMEOUT and costs 30 s, so a bad build makes the run long.
- It is on only when `RATE_KBIT` is set. `echo in prefetch` stays the prefetch scenario; it does not use the bulk.
- Combine `RATE_KBIT` with `lo_netem.sh` loss if needed. The proxy still enforces the cap.

The bulk pane itself produces only ~1.8 Mbit/s (the render slot holds one frame, so output coalesces), so a cap above ~2 Mbit/s never queues anything. Pick a cap below that. Fork build `63ad9b47` on both ends, RTT 66 ms, N=20, medians with p90 and max in ms:

| Cap | Echo idle | Echo under bulk | Bytes down in scenario |
| --- | --- | --- | --- |
| 8 Mbit/s | 71 (91, max 107) | 91 (112, max 129) | 7.5 MB, ~1.8 Mbit/s avg |
| 2 Mbit/s | 71 (100, max 134) | 100 (109, max 126) | 7.6 MB, ~1.8 Mbit/s avg |
| 1.5 Mbit/s | 70 (89, max 173) | 8344 (10942, max 10969) | 32.5 MB, 1.5 Mbit/s |
| 1 Mbit/s | 75 (89, max 98) | 14275 (16679), 13 of 20 timed out | 63 MB, 1.0 Mbit/s |
| 0.5 Mbit/s | 78 (90, max 97) | 18 of 20 timed out | 36 MB, 0.5 Mbit/s |

## Gotchas

- `HRIG_DIR` must stay short, 60 bytes at most. herdr's unix sockets live under the fake HOMEs, and macOS caps socket paths at 104 bytes. `$TMPDIR` is too long.
- One rig at a time. After a killed run, `rig.sh stop` cleans up.
- `ui_bench.py` runs through `uv run --script`, which fetches `pyte`. Pane focus clicks columns in whatever workspace is current. Pair it with the workspace flags, or attach a session that already shows the remote workspace.
- Against the real machine, use a throwaway session (see the `herdr-throwaway-repro` skill) with a remote workspace whose panes print a unique marker. Clicks change focus in whatever session the client attaches to.
- The `net_probe.py` UDP probe can congest its own path. Bursts closer than ~100 ms apart overflow the Mac's uplink queue and show up as up-direction loss that normal traffic never sees.
