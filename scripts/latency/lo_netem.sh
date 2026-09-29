#!/bin/bash
# Delay and packet loss on the latency rig's sshd port, using macOS dummynet on lo0.
#
# usage: sudo lo_netem.sh on RTT_MS LOSS_DOWN_PCT [LOSS_UP_PCT=0] [PORT=47022]
#        sudo lo_netem.sh off
#        lo_netem.sh status    show whether shaping is on (no root)
#        lo_netem.sh plan RTT_MS LOSS_DOWN_PCT [LOSS_UP_PCT=0] [PORT=47022]
#                              print the dnctl and pf commands `on` would run,
#                              syntax-checked, without touching the system (no root)
#
# Only TCP on lo0 to or from PORT is shaped, nothing else. "up" is client to
# server (packets to PORT), "down" is server to client (packets from PORT).
# Each direction gets RTT_MS/2 of delay and its own random per-packet loss.
# rig.sh picks the shaping up on its own when the state file matches its
# SSHD_PORT: run `on` first, then rig.sh, and `off` after `rig.sh stop`.
#
# Loss is dummynet's plr: independent per packet, not bursty. Segments are
# clamped to Tailscale's MSS so a screen update spans as many packets as on
# the real link.
#
# pf is enabled with `pfctl -E` and the returned token is kept in the state file,
# so `off` releases only this script's reference. Rules go into the anchor
# com.apple/herdr-netem, reached through the main ruleset's `scrub-anchor` and
# `dummynet-anchor "com.apple/*"` lines. `on` reloads the running main ruleset
# unchanged so those wildcards see the new anchor; /etc/pf.conf is never used.
set -uo pipefail

STATE=/tmp/herdr-latency-netem.state
ANCHOR=com.apple/herdr-netem
PIPE_UP=62001
PIPE_DOWN=62002
SELF=$0
ARGS=("$@")
DRY=0

die() { echo "lo_netem.sh: $*" >&2; exit 1; }

usage() { sed -n '2,/^set -uo/{/^set -uo/d;s/^# \{0,1\}//;p;}' "${BASH_SOURCE[0]}"; }

need_root() { [ "$(id -u)" = 0 ] || die "$1 needs root. Run: sudo $SELF ${ARGS[*]}"; }

is_number() { [[ $1 =~ ^[0-9]+(\.[0-9]+)?$ ]]; }
is_pct() { is_number "$1" && awk -v v="$1" 'BEGIN { exit !(v <= 100) }'; }

field() { tr ' ' '\n' <"$STATE" | sed -n "s/^$1=//p"; }

parse_shape() {
    RTT=${1:-}
    LOSS_DOWN=${2:-}
    LOSS_UP=${3:-0}
    PORT=${4:-47022}
    [ -n "$RTT" ] && [ -n "$LOSS_DOWN" ] || { usage >&2; exit 2; }
    is_number "$RTT" || die "RTT_MS must be a number, got '$RTT'"
    is_pct "$LOSS_DOWN" || die "LOSS_DOWN_PCT must be 0-100, got '$LOSS_DOWN'"
    is_pct "$LOSS_UP" || die "LOSS_UP_PCT must be 0-100, got '$LOSS_UP'"
    [[ $PORT =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "PORT must be 1-65535, got '$PORT'"
    HALF=$(awk -v r="$RTT" 'BEGIN { printf "%d", r / 2 + 0.5 }')
}

plr() { awk -v p="$1" 'BEGIN { printf "%.6f", p / 100 }'; }

# On lo0 every packet passes pf twice, once out and once in. Matching only
# `in` applies each delay and loss once.
# lo0's 16 KB MTU would carry a whole screen update in one segment, so loss per
# packet would hit far less often than on the real link. max-mss rewrites the
# handshake to Tailscale's MSS (1280 MTU - 52 bytes of IPv4 and TCP headers).
rules() {
    echo "scrub in on lo0 proto tcp from any to any port $PORT max-mss 1228"
    echo "scrub in on lo0 proto tcp from any port $PORT to any max-mss 1228"
    echo "dummynet in quick on lo0 proto tcp from any to any port $PORT pipe $PIPE_UP"
    echo "dummynet in quick on lo0 proto tcp from any port $PORT to any pipe $PIPE_DOWN"
}

# dnctl: print and syntax-check only (-n) in a dry run.
run() {
    if [ $DRY = 1 ]; then
        echo "+ $*"
        "$1" -n "${@:2}" || die "syntax check failed: $*"
    else
        "$@"
    fi
}

# The running main ruleset in pf.conf load order (scrub, nat/rdr, dummynet,
# filter), including anchors the system added after boot.
main_ruleset() {
    local filter
    filter=$(pfctl -s rules 2>/dev/null) || return 1
    grep '^scrub' <<<"$filter"
    pfctl -s nat 2>/dev/null
    pfctl -s dummynet 2>/dev/null
    grep -v '^scrub' <<<"$filter"
    return 0
}

load_rules() {
    if [ $DRY = 1 ]; then
        echo "+ pfctl -a $ANCHOR -f - <<EOF"
        rules | sed 's/^/    /'
        echo "  EOF"
        echo "+ main_ruleset | pfctl -f -   (reload the running main ruleset unchanged)"
        # pfctl -n warns about flushing the main ruleset even when it does not.
        rules | pfctl -n -a "$ANCHOR" -f - 2>&1 | grep -v -e 'flushing of rules' -e 'ruleset added by' -e '^See /etc/pf.conf' -e '^$'
        [ "${PIPESTATUS[1]}" = 0 ] || die "pf rule syntax check failed"
    else
        # A com.apple/* wildcard in the main ruleset only covers anchors that
        # existed when the main ruleset was loaded, so reload it, unchanged,
        # after creating ours.
        local main
        main=$(main_ruleset) && [ -n "$main" ] || die "cannot read the main pf ruleset"
        rules | pfctl -a "$ANCHOR" -f - && pfctl -q -f - <<<"$main"
    fi
}

remove_shaping() { # $1 = up pipe, $2 = down pipe
    pfctl -a "$ANCHOR" -f /dev/null 2>/dev/null
    dnctl pipe "$1" delete 2>/dev/null
    dnctl pipe "$2" delete 2>/dev/null
    return 0
}

cmd_on() {
    parse_shape "$@"
    if [ $DRY = 0 ]; then
        need_root on
        [ -f "$STATE" ] && die "already on ($(cat "$STATE")). Run: sudo $SELF off"
    fi
    if ! { run dnctl pipe "$PIPE_UP" config delay "${HALF}ms" plr "$(plr "$LOSS_UP")" \
        && run dnctl pipe "$PIPE_DOWN" config delay "${HALF}ms" plr "$(plr "$LOSS_DOWN")" \
        && load_rules; }; then
        [ $DRY = 1 ] || remove_shaping "$PIPE_UP" "$PIPE_DOWN"
        die "could not configure dummynet or load the pf rules"
    fi
    if [ $DRY = 1 ]; then
        echo "+ pfctl -E   (prints 'Token : N'; N goes into $STATE)"
        return 0
    fi
    local out token
    out=$(pfctl -E 2>&1)
    token=$(sed -n 's/.*Token : *\([0-9][0-9]*\).*/\1/p' <<<"$out")
    if [ -z "$token" ]; then
        remove_shaping "$PIPE_UP" "$PIPE_DOWN"
        die "pfctl -E gave no token, so pf may now be enabled with no way to release it. Output: $out"
    fi
    echo "port=$PORT rtt=$RTT loss_down=$LOSS_DOWN loss_up=$LOSS_UP pipe_up=$PIPE_UP pipe_down=$PIPE_DOWN token=$token" >"$STATE.tmp"
    chmod 644 "$STATE.tmp"
    mv "$STATE.tmp" "$STATE"
    echo "netem on: port $PORT, RTT $RTT ms, loss down $LOSS_DOWN% up $LOSS_UP% (remove with: sudo $SELF off)"
    dnctl pipe show
}

cmd_off() {
    need_root off
    local up=$PIPE_UP down=$PIPE_DOWN token=
    if [ -f "$STATE" ]; then
        up=$(field pipe_up)
        down=$(field pipe_down)
        token=$(field token)
        [[ $up =~ ^[0-9]+$ && $down =~ ^[0-9]+$ ]] || die "bad state file $STATE; delete it and run off again"
    fi
    remove_shaping "$up" "$down"
    if [[ $token =~ ^[0-9]+$ ]]; then
        pfctl -X "$token" 2>/dev/null || echo "lo_netem.sh: pf token $token was already released" >&2
    fi
    rm -f "$STATE"
    echo "netem off"
}

cmd_status() {
    [ -f "$STATE" ] || { echo "netem off (no state file)"; return 0; }
    echo "netem on: $(cat "$STATE")"
    local boot
    boot=$(sysctl -n kern.boottime | sed -n 's/^{ sec = \([0-9]*\),.*/\1/p')
    if [ -n "$boot" ] && [ "$(stat -f %m "$STATE")" -lt "$boot" ]; then
        echo "STALE: the state file is older than the last boot, so the pf rules and pipes are gone."
        echo "Clean up with: sudo $SELF off"
    else
        echo "Only root can check the kernel: sudo dnctl pipe show; sudo pfctl -a $ANCHOR -s rules"
    fi
}

case "${1:-}" in
    -h | --help | help) usage; exit 0 ;;
    on) shift; cmd_on "$@" ;;
    off) cmd_off ;;
    status) cmd_status ;;
    plan) shift; DRY=1; cmd_on "$@" ;;
    *) usage >&2; exit 2 ;;
esac
