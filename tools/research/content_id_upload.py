"""Request shape for the engine 3.2.11 HLS content-id upload.

The original client posts the base64 of a wrapped AceStream transport to
``http://api.torrentstream.net/upload/raw``. Publishing adds ``publish=1`` and the
swarm infohash. The content id is the JSON string ``content_id`` in the response.
``player_id`` and ``id`` are not that field.

This module does not contact the network and does not claim the production catalog
accepts the request. See docs/protocol/notes/75-public-content-id-lifecycle.md.
"""

from __future__ import annotations

import base64
import json
from urllib.parse import urlencode

UPLOAD_HOST = "api.torrentstream.net"
UPLOAD_PATH = "/upload/raw"


def encode_upload_body(transport: bytes) -> bytes:
    """Return the exact body bytes the engine sent for a wrapped transport."""
    if not transport.startswith(b"AceStreamTransport"):
        raise ValueError("upload body is the base64 of a wrapped AceStream transport")
    return base64.b64encode(transport)


def upload_target(*, publish: bool, infohash: str | None = None) -> str:
    """Return the path and query, without a scheme or host."""
    if not publish:
        if infohash is not None:
            raise ValueError("a lookup does not send an infohash")
        return UPLOAD_PATH
    if infohash is None or len(infohash) != 40 or any(c not in "0123456789abcdef" for c in infohash):
        raise ValueError("publish requires a lowercase hex swarm infohash")
    return UPLOAD_PATH + "?" + urlencode((("publish", "1"), ("infohash", infohash)))


def content_id_from_upload_json(payload: object) -> str | None:
    """Return the string the engine copied into its reported content id.

    The observed parser reads the top-level ``content_id`` string and ignores
    ``player_id`` and ``id``. It does not document a length or alphabet check;
    the engine echoed the string it was given.
    """
    if not isinstance(payload, dict):
        return None
    value = payload.get("content_id")
    if not isinstance(value, str):
        return None
    return value


def content_id_from_upload_body(body: bytes) -> str | None:
    try:
        payload = json.loads(body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    return content_id_from_upload_json(payload)
