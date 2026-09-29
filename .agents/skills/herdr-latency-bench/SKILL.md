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

## Comparing builds with the rig

The rig is a local sshd behind `delay_proxy.py`, a fixed-delay TCP proxy, with fake client and remote HOMEs. It never touches the real server, `~/.config/herdr` or `~/.ssh`.

1. Build the candidate: `cargo build --release`. The rig uses `target/release/herdr` for the client and the local server unless `HERDR_BIN` is set.
2. Run a baseline, then the candidate, the same way. Save both with `HRIG_JSON=/tmp/<name>.json`, for example `HERDR_BIN=<baseline build> HRIG_JSON=/tmp/base.json scripts/latency/rig.sh run 66 10`.
3. Compare visible and settled medians, the `turns` column (round trips per action), and wire bytes. At a fixed delay, `turns` and bytes are the numbers a change actually moves. The ms figures follow from them.

Set `HERDR_REMOTE_BIN` to a fork build when the change is on the remote side. By default the remote runs the `herdr` on PATH.

Baseline at 66 ms, fork build on both ends (`65a3e8a5`), medians with p90 in brackets:

| Action | Loss | Visible | Settled | Turns | Bytes down |
| --- | --- | --- | --- | --- | --- |
| Switch to remote | none | ~105 ms | ~260 ms | 3 | ~14 KB |
| Switch to remote | 1% down | ~107 ms (131) | ~264 ms (383) | 3 | ~14 KB |
| Pane focus | none | ~78 ms | ~85 ms | 1 | ~0.9 KB |
| Pane focus | 1% down | ~77 ms (94) | ~84 ms (106) | 1 | ~0.9 KB |

The goal is local speed: ~27 ms to switch to a local workspace, with no link traffic. Loss moves the tail, not the median, so judge loss runs by p90 and max with `N` of 30 or more.

## Adding packet loss

The real jan-box link loses ~1%, mostly server to client, and TCP turns one lost segment into 250 ms to several seconds of recovery. The proxy can't drop segments, so `lo_netem.sh` adds delay and loss in the kernel (macOS dummynet) on the rig's sshd port only, and clamps segments to Tailscale's size so a screen update spans as many packets as on the real link.

1. `sudo scripts/latency/lo_netem.sh on 66 1`: RTT, loss down %, optional loss up %.
2. `scripts/latency/rig.sh run 66 30`. The rig prints a `shaping:` line and a `shaped path check`; the check must read ~66 ms and `mss 1216`, or the shaping is not active.
3. `rig.sh stop` if you used `setup`, then `sudo scripts/latency/lo_netem.sh off`.

Dummynet loss is random per packet; the real link drops back-to-back segments, so the real tail is worse. `lo_netem.sh status` shows whether shaping is on.

## Gotchas

- `HRIG_DIR` must stay short, 60 bytes at most. herdr's unix sockets live under the fake HOMEs, and macOS caps socket paths at 104 bytes. `$TMPDIR` is too long.
- One rig at a time. After a killed run, `rig.sh stop` cleans up.
- `ui_bench.py` runs through `uv run --script`, which fetches `pyte`. Pane focus clicks columns in whatever workspace is current. Pair it with the workspace flags, or attach a session that already shows the remote workspace.
- Against the real machine, use a throwaway session (see the `herdr-throwaway-repro` skill) with a remote workspace whose panes print a unique marker. Clicks change focus in whatever session the client attaches to.
- The `net_probe.py` UDP probe can congest its own path. Bursts closer than ~100 ms apart overflow the Mac's uplink queue and show up as up-direction loss that normal traffic never sees.
