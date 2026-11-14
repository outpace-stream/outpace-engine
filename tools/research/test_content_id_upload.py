import importlib.util
import json
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "content_id_upload", Path(__file__).with_name("content_id_upload.py")
)
upload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(upload)

FIXTURES = Path(__file__).with_name("fixtures")
INFOHASH = "0123456789abcdef0123456789abcdef01234567"
TRANSPORT = b"AceStreamTransport" + b"\x00\x02" + b"synthetic-body"


class UploadRequestTests(unittest.TestCase):
    def test_lookup_posts_base64_without_a_query(self):
        body = upload.encode_upload_body(TRANSPORT)
        self.assertEqual(body, b"QWNlU3RyZWFtVHJhbnNwb3J0AAJzeW50aGV0aWMtYm9keQ==")
        self.assertEqual(upload.upload_target(publish=False), "/upload/raw")

    def test_publish_adds_the_swarm_infohash_and_not_a_content_id(self):
        target = upload.upload_target(publish=True, infohash=INFOHASH)
        self.assertEqual(
            target,
            "/upload/raw?publish=1&infohash=0123456789abcdef0123456789abcdef01234567",
        )
        self.assertNotIn("content_id", target)

    def test_publish_rejects_a_missing_or_uppercase_infohash(self):
        with self.assertRaises(ValueError):
            upload.upload_target(publish=True)
        with self.assertRaises(ValueError):
            upload.upload_target(publish=True, infohash=INFOHASH.upper())

    def test_response_uses_only_the_content_id_field(self):
        accepted = json.loads((FIXTURES / "upload-raw-content-id.json").read_text())
        ignored = json.loads((FIXTURES / "upload-raw-other-fields.json").read_text())
        self.assertEqual(
            upload.content_id_from_upload_json(accepted),
            "0123456789abcdef0123456789abcdef01234567",
        )
        self.assertIsNone(upload.content_id_from_upload_json(ignored))
        self.assertIsNone(upload.content_id_from_upload_body(b"not-json"))
        self.assertIsNone(upload.content_id_from_upload_body(b"\xff"))


if __name__ == "__main__":
    unittest.main()
