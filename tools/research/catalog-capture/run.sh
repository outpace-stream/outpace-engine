#!/bin/sh
# Runs INSIDE `unshare -rnm`: private network namespace with only loopback, so nothing can
# leave this machine. Captures what engine 3.2.11 sends when asked for a broadcast content id.
set -u
# usage: unshare -rnm tools/research/catalog-capture/run.sh WORKDIR
#   ENGINE_DIR  engine 3.2.11 tree (default: the swarmtest cache)
#   PY310       a CPython 3.10 prefix providing libpython3.10.so and the stdlib
#   VENV        a venv with the engine's requirements.txt installed
#   CAP_TRANSPORT  optional live transport to register instead of a fresh outpace broadcast
# The outpace binary is taken from target/release; build it first.
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
T=${1:?usage: run.sh WORKDIR}
E=${ENGINE_DIR:-$HOME/.cache/outpace-swarmtest/engine-3.2.11}
P=${PY310:?set PY310 to a CPython 3.10 prefix}
VENV=${VENV:?set VENV to a venv with the engine requirements}
WT=$REPO
mkdir -p "$T"
T=$(cd "$T" && pwd)
rm -rf "$T/out" && mkdir -p "$T/out" "$T/state" "$T/cache" "$T/od"

ip link set lo up
printf 'nameserver 127.0.0.1\n' > "$T/resolv.conf"
printf '127.0.0.1 localhost\n::1 localhost\n' > "$T/hosts"
printf 'passwd: files\ngroup: files\nhosts: files dns\n' > "$T/nsswitch.conf"
mount --bind "$T/resolv.conf" /etc/resolv.conf
mount --bind "$T/hosts" /etc/hosts
mount --bind "$T/nsswitch.conf" /etc/nsswitch.conf
# Loopback-only default route: catalog IPs are redirected to the mock, everything else is refused
# locally. There is still no interface that leads off this machine.
ip route add default dev lo
nft -f - <<'NFT'
table ip cap {
  chain out {
    type nat hook output priority -100;
    ip daddr != 127.0.0.0/8 tcp dport { 80, 8080, 8081 } redirect to :80
  }
}
NFT
ip route show > "$T/out/routes.txt"
ip link show > "$T/out/links.txt"

# 1. A throwaway live broadcast from outpace, offline, to get a live BT transport file.
OUTPACE_BIND=127.0.0.1:16878 OUTPACE_RTMP_BIND=127.0.0.1:11935 OUTPACE_PEER_LISTEN=127.0.0.1:18631 \
OUTPACE_DATA_DIR=$T/od OUTPACE_CACHE_DIR=$T/od/cache \
  "$WT/target/release/outpace" broadcast capture-test > "$T/out/outpace.log" 2>&1 &
OP=$!
sleep 4
curl -s -o "$T/out/capture.acelive" http://127.0.0.1:16878/broadcast/capture-test
kill $OP; wait $OP 2>/dev/null
if [ -n "${CAP_TRANSPORT:-}" ]; then
  cp "$CAP_TRANSPORT" "$T/out/capture.acelive" || {
    echo "cannot copy CAP_TRANSPORT=$CAP_TRANSPORT" >&2
    exit 1
  }
fi
ls -la "$T/out/capture.acelive" > "$T/out/transport.txt" 2>&1

# 2. Mock catalog, DNS and TLS sniffer.
python3 "$HERE/mock.py" "$T/out" "$T/out/capture.acelive" > "$T/out/mock.log" 2>&1 &
MP=$!
sleep 1

# 3. The original engine, console client.
cd "$E"
PYTHONHOME=$P PYTHONPATH=$VENV/lib/python3.10/site-packages LD_LIBRARY_PATH=$E/lib:$P/lib \
  ./acestreamengine --client-console --log-stderr --disable-sentry \
  --state-dir "$T/state" --cache-dir "$T/cache" > "$T/out/engine.log" 2>&1 &
EP=$!
for i in $(seq 1 60); do
  curl -s -o /dev/null http://127.0.0.1:6878/webui/api/service?method=get_version && break
  sleep 1
done
api() { curl -s --max-time 60 "http://127.0.0.1:6878/server/api?api_version=3&$1"; echo; }
TOKEN=$(python3 -c "import json;print(json.load(open('$E/engine_runtime.json'))['api_access_token'])" 2>/dev/null)
echo "token-present=$([ -n "$TOKEN" ] && echo yes || echo no)" > "$T/out/api.txt"
{
  echo "## get_version"; api "method=get_version&token=$TOKEN"
  echo "## playlist_add_item"; api "method=playlist_add_item&token=$TOKEN&url=http%3A%2F%2F127.0.0.1%2Fcapture.acelive&title=capture-test&category=other"
  sleep 3
  echo "## playlist_get"; api "method=playlist_get&token=$TOKEN"
} >> "$T/out/api.txt" 2>&1
IH=$(grep -oE 'Infohash: [0-9a-f]{40}' "$T/out/outpace.log" | head -1 | cut -c11-)
ITEM=$(python3 - "$T/out/api.txt" <<'EOF'
import json, re, sys
txt = open(sys.argv[1]).read().split("## playlist_get", 1)[-1]
for line in txt.splitlines():
    line = line.strip()
    if line.startswith("{"):
        try:
            d = json.loads(line)
        except Exception:
            continue
        found = re.findall(r'"id":\s*"?([0-9]+)"?', json.dumps(d))
        print(found[0] if found else "")
        break
EOF
)
{
  echo "## item=$ITEM"
  echo "## get_content_id by playlist_item_id"; api "method=get_content_id&token=$TOKEN&playlist_item_id=$ITEM"
  echo "## get_content_id by infohash"; api "method=get_content_id&token=$TOKEN&infohash=$IH"
} >> "$T/out/api.txt" 2>&1
sleep 5
kill $EP $MP 2>/dev/null
wait 2>/dev/null
echo done > "$T/out/done"
