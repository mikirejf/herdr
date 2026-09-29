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

Baseline at 66 ms, fork `f0f13de5` against stock 0.9.1 on the remote:

| Action | Visible | Settled | Turns | Bytes down |
| --- | --- | --- | --- | --- |
| Switch to remote | ~110 ms | ~270 ms | 3 | ~14 KB |
| Pane focus | ~77 ms | ~84 ms | 1 | ~0.9 KB |

The goal is local speed: ~27 ms to switch to a local workspace, with no link traffic.

## Gotchas

- The rig has delay only. It has no loss or bandwidth cap: a userspace TCP proxy can't drop segments the way a real link does. The real jan-box link loses ~1% in bursts, and that loss turns one lost segment into 250 ms to several seconds of TCP recovery. Check loss-sensitive designs against the real machine as well.
- `HRIG_DIR` must stay short, 60 bytes at most. herdr's unix sockets live under the fake HOMEs, and macOS caps socket paths at 104 bytes. `$TMPDIR` is too long.
- One rig at a time. After a killed run, `rig.sh stop` cleans up.
- `ui_bench.py` runs through `uv run --script`, which fetches `pyte`. Pane focus clicks columns in whatever workspace is current. Pair it with the workspace flags, or attach a session that already shows the remote workspace.
- Against the real machine, use a throwaway session (see the `herdr-throwaway-repro` skill) with a remote workspace whose panes print a unique marker. Clicks change focus in whatever session the client attaches to.
- The `net_probe.py` UDP probe can congest its own path. Bursts closer than ~100 ms apart overflow the Mac's uplink queue and show up as up-direction loss that normal traffic never sees.
