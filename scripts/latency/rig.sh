#!/bin/bash
# Fixed-delay local SSH rig for measuring herdr UI latency (macOS).
#
# usage: rig.sh run [RTT_MS=66] [N=5]   build the rig, run ui_bench.py, tear down
#        rig.sh setup [RTT_MS=66]       build the rig and leave it running
#        rig.sh stop                    tear down (also cleans a killed run) and delete HRIG_DIR
#
# The rig: a user-level sshd on 127.0.0.1:SSHD_PORT, a delay_proxy.py in front
# of it on PROXY_PORT (RTT_MS round trip; 66 ms is the real Mac <-> jan-box
# minimum), a fake remote HOME with its own herdr server and a two-pane
# workspace, and a fake client HOME with a local server, a local workspace and
# the proxy saved as a machine. Both homes run herdr defaults. Every herdr call
# runs under `env -i HOME=<fake home>`, so the real server, ~/.config/herdr and
# ~/.ssh are never touched.
#
# env:   HERDR_BIN         client and local server build (default: this repo's
#                          target/release/herdr if it exists, else herdr on PATH)
#        HERDR_REMOTE_BIN  build the fake remote machine runs (default: herdr on PATH)
#        SSHD_PORT / PROXY_PORT   default 47022 / 47023
#        HRIG_DIR          state dir (default /tmp/herdr-latency-rig). Keep it short
#                          and without spaces: herdr's unix sockets live under it
#                          and macOS limits socket paths to 103 bytes.
#        HRIG_JSON         also write ui_bench.py's raw samples to this file
# Not supported: two rigs at once with the same ports or HRIG_DIR.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO=$(cd "$HERE/../.." && pwd -P)
HRIG_DIR=${HRIG_DIR:-/tmp/herdr-latency-rig}
SSHD_PORT=${SSHD_PORT:-47022}
PROXY_PORT=${PROXY_PORT:-47023}
SAFE_PATH=/usr/bin:/bin:/usr/sbin:/sbin
CLIENT_HOME=$HRIG_DIR/home-client
REMOTE_HOME=$HRIG_DIR/home-remote
REMOTE_BIN=$REMOTE_HOME/.local/bin/herdr
KEYS=$HRIG_DIR/keys
RUN=$HRIG_DIR/run
MARKER_FILE=$HRIG_DIR/.herdr-latency-rig
KEEP=0

die() { echo "rig.sh: $*" >&2; exit 1; }

usage() { sed -n '2,/^set -uo/{/^set -uo/d;s/^# \{0,1\}//;p;}' "${BASH_SOURCE[0]}"; }

case "${1:-}" in
    -h | --help | help) usage; exit 0 ;;
esac
COMMAND=${1:-}
[ $# -gt 0 ] && shift
case $COMMAND in run | setup | stop) ;; *) usage >&2; exit 2 ;; esac
RTT=${1:-66}
N=${2:-5}
[[ $RTT =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "RTT_MS must be a number, got '$RTT'"
[[ $N =~ ^[1-9][0-9]*$ ]] || die "N must be a positive integer, got '$N'"

case $HRIG_DIR in
    "" | / | "$HOME" | "$HOME"/) die "refusing HRIG_DIR='$HRIG_DIR'" ;;
    /*[[:space:]]*) die "HRIG_DIR must not contain whitespace" ;;
    /*) ;;
    *) die "HRIG_DIR must be an absolute path" ;;
esac
# Longest socket under it is $CLIENT_HOME/.config/herdr/herdr.sock.
[ ${#HRIG_DIR} -le 60 ] || die "HRIG_DIR is too long for unix socket paths (max 60 bytes)"

PYTHON=$(command -v python3) || die "python3 not found"

# Every herdr call goes through env -i so no HERDR_*/XDG_* variable of the
# calling session can point it at the real server or config.
isolated() { local home=$1; shift; env -i HOME="$home" PATH=$SAFE_PATH TERM=xterm-256color "$@"; }
client_herdr() { isolated "$CLIENT_HOME" "$CLIENT_BIN" "$@"; }
remote_herdr() { isolated "$REMOTE_HOME" "$REMOTE_BIN" "$@"; }

json_field() { "$PYTHON" -c 'import json, sys
d = json.load(sys.stdin)["result"]
for key in sys.argv[1].split("."):
    d = d[key]
print(d)' "$1"; }

descendants() {
    ps -axo pid=,ppid= | awk -v root="$1" '
        { kids[$2] = kids[$2] " " $1 }
        function walk(p,   n, i, a) { n = split(kids[p], a, " "); for (i = 1; i <= n; i++) { print a[i]; walk(a[i]) } }
        END { walk(root) }'
}

# A pidfile holds "<pid> <text the process command line must contain>" so a
# stale file can never make us kill an unrelated process that reused the pid.
kill_pidfile() {
    [ -f "$1" ] || return 0
    local pid match cmd kids
    read -r pid match <"$1"
    cmd=$(ps -p "$pid" -o command= 2>/dev/null)
    if [ -n "$pid" ] && [ -n "$match" ] && [[ $cmd == *"$match"* ]]; then
        kids=$(descendants "$pid")
        kill "$pid" $kids 2>/dev/null
        for _ in $(seq 1 20); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        # The attached herdr client ignores SIGTERM.
        kill -9 "$pid" $kids 2>/dev/null
    fi
    rm -f "$1"
}

stop_herdr_server() { # $1 = home, $2 = binary, $3 = pidfile name
    if [ -S "$1/.config/herdr/herdr.sock" ] && [ -x "$2" ]; then
        isolated "$1" "$2" server stop >/dev/null 2>&1
        for _ in $(seq 1 50); do [ -S "$1/.config/herdr/herdr.sock" ] || break; sleep 0.1; done
    fi
    kill_pidfile "$RUN/$3.pid"
}

teardown() {
    [ -d "$RUN" ] || return 0
    kill_pidfile "$RUN/bench.pid"
    stop_herdr_server "$CLIENT_HOME" "$(cat "$RUN/client-bin" 2>/dev/null)" local-server
    stop_herdr_server "$REMOTE_HOME" "$REMOTE_BIN" remote-server
    # ControlPersist keeps ssh mux masters alive; they are the only ssh
    # clients talking to the proxy port.
    local masters
    masters=$(lsof -nP -a -c ssh -iTCP@127.0.0.1:"$PROXY_PORT" -t 2>/dev/null | sort -u)
    [ -n "$masters" ] && kill $masters 2>/dev/null
    sleep 0.3
    kill_pidfile "$RUN/proxy.pid"
    kill_pidfile "$RUN/sshd.pid"
}

remove_state_dir() {
    [ -e "$HRIG_DIR" ] || return 0
    if [ -n "$(ls -A "$HRIG_DIR" 2>/dev/null)" ] && [ ! -f "$MARKER_FILE" ]; then
        die "refusing to delete $HRIG_DIR: rig.sh did not create it"
    fi
    rm -rf "$HRIG_DIR"
}

on_exit() { [ $KEEP = 1 ] || teardown; }
trap on_exit EXIT
trap 'exit 130' INT TERM

start_detached() { # $1 = pidfile name, $2 = expected cmdline text, $3 = HOME, $4 = log, rest = command
    local name=$1 match=$2 home=$3 log=$4
    shift 4
    # A session leader: herdr servers require one, and it keeps hangups away.
    env -i HOME="$home" PATH=$SAFE_PATH TERM=xterm-256color \
        "$PYTHON" -c 'import os, sys; os.setsid(); os.execv(sys.argv[1], sys.argv[1:])' "$@" \
        >"$log" 2>&1 &
    echo "$! $match" >"$RUN/$name.pid"
}

wait_for() { # $1 = seconds, rest = test command
    local secs=$1
    shift
    for _ in $(seq 1 $((secs * 10))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.1; done
    return 1
}

server_running() { "$@" status server --json | grep -q '"running":true'; }

resolve_binaries() {
    if [ -z "${HERDR_BIN:-}" ]; then
        if [ -x "$REPO/target/release/herdr" ]; then
            HERDR_BIN=$REPO/target/release/herdr
        else
            HERDR_BIN=$(command -v herdr) || die "no herdr build: set HERDR_BIN or run cargo build --release"
        fi
    fi
    HERDR_REMOTE_BIN=${HERDR_REMOTE_BIN:-$(command -v herdr)} \
        || die "no remote herdr on PATH: set HERDR_REMOTE_BIN"
    [ -x "$HERDR_BIN" ] || die "HERDR_BIN is not executable: $HERDR_BIN"
    [ -x "$HERDR_REMOTE_BIN" ] || die "HERDR_REMOTE_BIN is not executable: $HERDR_REMOTE_BIN"
    CLIENT_BIN=$HERDR_BIN
}

describe_binary() {
    printf '%s (%s, built %s)' "$1" "$(isolated "$HRIG_DIR" "$1" --version 2>&1)" \
        "$(date -r "$1" '+%Y-%m-%d %H:%M')"
}

write_config() {
    # One host key for the fake sshd, one client key it accepts.
    ssh-keygen -q -t ed25519 -N '' -f "$KEYS/client_key" -C hrig || die "ssh-keygen failed"
    ssh-keygen -q -t ed25519 -N '' -f "$KEYS/host_key" -C hrig-host || die "ssh-keygen failed"
    cp "$KEYS/client_key.pub" "$KEYS/authorized_keys"
    echo "hrig-remote $(cut -d' ' -f1,2 "$KEYS/host_key.pub")" >"$KEYS/known_hosts"
    chmod 600 "$KEYS/client_key" "$KEYS/host_key" "$KEYS/authorized_keys"

    # sshd starts sessions with HOME from the passwd entry; point them at the
    # fake remote home and drop anything that could reach the real herdr.
    cat >"$HRIG_DIR/remote-shell.sh" <<EOF
#!/bin/sh
for v in \$(env | sed -n 's/^\(HERDR_[A-Za-z0-9_]*\)=.*/\1/p'); do unset "\$v"; done
unset XDG_CONFIG_HOME XDG_STATE_HOME
export HOME=$REMOTE_HOME
export PATH=$SAFE_PATH
cd "\$HOME"
if [ -n "\$SSH_ORIGINAL_COMMAND" ]; then exec /bin/sh -c "\$SSH_ORIGINAL_COMMAND"; fi
exec /bin/sh -l
EOF
    chmod +x "$HRIG_DIR/remote-shell.sh"

    cat >"$HRIG_DIR/sshd_config" <<EOF
ListenAddress 127.0.0.1
HostKey $KEYS/host_key
PidFile $RUN/sshd-listener.pid
AuthorizedKeysFile $KEYS/authorized_keys
UsePAM no
StrictModes no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PermitTTY yes
ForceCommand $HRIG_DIR/remote-shell.sh
LogLevel ERROR
EOF

    # herdr includes $HOME/.ssh/config (src/platform/unix_common.rs), so the
    # fake client HOME carries the host alias; the real ~/.ssh is never read.
    cat >"$CLIENT_HOME/.ssh/config" <<EOF
Host hrig-remote
  HostName 127.0.0.1
  Port $PROXY_PORT
  User $(id -un)
  HostKeyAlias hrig-remote
  IdentityFile $KEYS/client_key
  IdentitiesOnly yes
  IdentityAgent none
  UserKnownHostsFile $KEYS/known_hosts
  GlobalKnownHostsFile /dev/null
EOF
    echo 'onboarding = false' >"$CLIENT_HOME/.config/herdr/config.toml"
    # A plain prompt, so the user's zsh setup never matters.
    echo "PS1='%~ %# '" >"$CLIENT_HOME/.zshrc"
    echo "PS1='%~ %# '" >"$REMOTE_HOME/.zshrc"
    # A copy, not a symlink: herdr may replace the remote binary during
    # machine setup and must never be able to touch the original.
    cp "$HERDR_REMOTE_BIN" "$REMOTE_BIN" || die "cannot copy remote binary"
    echo "$CLIENT_BIN" >"$RUN/client-bin"
}

setup() {
    teardown
    remove_state_dir
    mkdir -p "$RUN" "$KEYS" "$CLIENT_HOME/.ssh" "$CLIENT_HOME/.config/herdr" "$REMOTE_HOME/.local/bin"
    : >"$MARKER_FILE"
    : >"$RUN/trace.log"
    write_config

    /usr/sbin/sshd -t -f "$HRIG_DIR/sshd_config" || die "sshd config invalid"
    start_detached sshd "$HRIG_DIR/sshd_config" "$HRIG_DIR" "$RUN/sshd.log" \
        /usr/sbin/sshd -f "$HRIG_DIR/sshd_config" -D -p "$SSHD_PORT"
    start_detached proxy delay_proxy.py "$HRIG_DIR" "$RUN/proxy.log" \
        "$PYTHON" "$HERE/delay_proxy.py" "$PROXY_PORT" "$SSHD_PORT" "$RTT" "$RUN/trace.log"
    wait_for 5 nc -z 127.0.0.1 "$SSHD_PORT" || die "sshd did not start (see $RUN/sshd.log)"
    wait_for 5 nc -z 127.0.0.1 "$PROXY_PORT" || die "proxy did not start (see $RUN/proxy.log)"
    "$PYTHON" "$HERE/delay_proxy.py" --selftest "$RTT" || die "proxy selftest failed"

    # Remote side: a workspace with two panes side by side showing distinct
    # text. The markers are printed with printf so the typed command line
    # never contains them; only real output does.
    start_detached remote-server "$REMOTE_BIN" "$REMOTE_HOME" "$RUN/remote-server.log" "$REMOTE_BIN" server
    wait_for 10 server_running remote_herdr || die "remote server did not start (see $RUN/remote-server.log)"
    local ws left right
    ws=$(remote_herdr workspace create --cwd "$REPO" --label rigremote --no-focus) || die "remote workspace"
    left=$(json_field root_pane.pane_id <<<"$ws") || die "cannot read remote pane id from: $ws"
    right=$(remote_herdr pane split "$left" --direction right --no-focus | json_field pane.pane_id) \
        || die "remote split"
    remote_herdr pane run "$left" "printf 'LEFT-PANE-%s\n' MARKER; git --no-pager log --oneline -15" >/dev/null \
        || die "remote pane run (left)"
    remote_herdr pane run "$right" 'for i in $(seq 1 40); do echo RIGHT-PANE-LINE $i; done' >/dev/null \
        || die "remote pane run (right)"
    remote_herdr pane wait-output "$left" --match LEFT-PANE-MARKER --timeout 10000 >/dev/null \
        || die "remote left pane text missing"
    remote_herdr pane wait-output "$right" --match 'RIGHT-PANE-LINE 40' --timeout 10000 >/dev/null \
        || die "remote right pane text missing"

    # Client side: local server with one local workspace, then the saved
    # machine that points at the delayed sshd.
    start_detached local-server "$CLIENT_BIN" "$CLIENT_HOME" "$RUN/local-server.log" "$CLIENT_BIN" server
    wait_for 10 server_running client_herdr || die "local server did not start (see $RUN/local-server.log)"
    ws=$(client_herdr workspace create --cwd /tmp --label local --no-focus) || die "local workspace"
    left=$(json_field root_pane.pane_id <<<"$ws") || die "cannot read local pane id from: $ws"
    client_herdr pane run "$left" "printf 'LOCAL-PANE-%s\n' MARKER" >/dev/null || die "local pane run"
    client_herdr pane wait-output "$left" --match LOCAL-PANE-MARKER --timeout 10000 >/dev/null \
        || die "local pane text missing"
    client_herdr machine add hrig-remote --label rigremote >"$RUN/machine-add.log" 2>&1 || {
        cat "$RUN/machine-add.log" >&2
        die "machine add failed"
    }
}

client_command() { echo env -i HOME="$CLIENT_HOME" PATH=$SAFE_PATH TERM=xterm-256color "$CLIENT_BIN"; }

bench() {
    local json=()
    [ -n "${HRIG_JSON:-}" ] && json=(--json "$HRIG_JSON")
    # Background + wait, so the EXIT/TERM trap can run while it is going.
    "$HERE/ui_bench.py" -n "$N" \
        --remote-ws '· rigremote' --remote-marker LEFT-PANE-MARKER \
        --local-ws '· local' --local-marker LOCAL-PANE-MARKER \
        --pane-cols 60,150 --trace "$RUN/trace.log" ${json[@]+"${json[@]}"} \
        --label "RTT $RTT ms, client $CLIENT_BIN, remote $HERDR_REMOTE_BIN" \
        -- $(client_command) &
    echo "$! ui_bench.py" >"$RUN/bench.pid"
    wait $!
    local status=$?
    rm -f "$RUN/bench.pid"
    return $status
}

leftovers() {
    local found
    found=$(ps -axo pid=,command= | grep -F -- "$HRIG_DIR" | grep -v -e grep -e "rig.sh" | grep -v "^ *$$ ")
    [ -S "$CLIENT_HOME/.config/herdr/herdr.sock" ] && found+=$'\n'"local herdr server socket still present"
    [ -S "$REMOTE_HOME/.config/herdr/herdr.sock" ] && found+=$'\n'"remote herdr server socket still present"
    [ -z "${found//[[:space:]]/}" ] && return 0
    echo "rig.sh: still running after stop:$found" >&2
    return 1
}

if [ "$COMMAND" = stop ]; then
    teardown
    leftovers || exit 1
    remove_state_dir
    echo "rig stopped"
    exit 0
fi

resolve_binaries
echo "herdr latency rig: RTT $RTT ms, N=$N, state dir $HRIG_DIR"
echo "  client + local server: $(describe_binary "$CLIENT_BIN")"
echo "  remote:                $(describe_binary "$HERDR_REMOTE_BIN")"
echo "  repo HEAD:             $(git -C "$REPO" log -1 --format='%h %cd' --date=format:'%Y-%m-%d %H:%M' 2>/dev/null)"
setup

if [ "$COMMAND" = setup ]; then
    KEEP=1
    echo "rig is up (stop with: $0 stop). Attach a client with:"
    echo "  $(client_command)"
    echo "or benchmark it with:"
    echo "  $HERE/ui_bench.py --pane-cols 60,150 --trace $RUN/trace.log -- $(client_command)"
    exit 0
fi

bench
