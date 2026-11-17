# catalog-capture

Captures what the original engine 3.2.11 sends to the AceStream catalog when you ask it for a
broadcast's content id. Protocol note 76 was produced with it.

Everything runs inside `unshare -rnm`: a private user, mount and network namespace whose only
interface is loopback. `/etc/resolv.conf`, `/etc/hosts` and `/etc/nsswitch.conf` are
bind-mounted so every name resolves to the mock, and an nft rule redirects catalog ports
(80, 8080, 8081) on any address to it. Nothing can reach the real catalog.

1. An offline `outpace broadcast capture-test` provides a live transport. Alternatively,
   `CAP_TRANSPORT` names one, such as `tests/vectors/transport/synthetic-live-512k.bin`.
2. `mock.py` serves that transport, answers DNS, logs every HTTP request byte for byte, and
   signs catalog replies the way the catalog does (note 62).
3. The engine runs as a console client. The script adds the transport to its playlist and
   calls `get_content_id`, which triggers `checktorrent` and then `puttorrent`.

```bash
cargo build --release -p ace-engine --bin outpace
python3.10 -m venv /tmp/acevenv && /tmp/acevenv/bin/pip install -r <engine>/requirements.txt
PY310=<cpython-3.10 prefix> VENV=/tmp/acevenv \
CAP_TRANSPORT=tests/vectors/transport/synthetic-live-512k.bin \
  unshare -rnm tools/research/catalog-capture/run.sh target/catcap
less target/catcap/out/requests.jsonl target/catcap/out/api.txt
```

The engine tree is the swarmtest cache (`ENGINE_DIR`) and is not in the repository. Keep the
output private: with a fresh outpace broadcast it contains that broadcast's derived hashes.
