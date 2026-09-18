#!/usr/bin/env python3
"""Render a template via templater's HTTP API, writing the output to a
presigned S3 PUT URL (see examples/render-to-s3.json).

Settings via environment (standard AWS names, picked up by boto3 itself
except AWS_S3_FORCE_PATH_STYLE, which botocore doesn't read on its own):
  AWS_ACCESS_KEY_ID       required
  AWS_SECRET_ACCESS_KEY   required
  AWS_SESSION_TOKEN       optional
  AWS_REGION              optional, default "us-east-1"
  AWS_ENDPOINT_URL        optional, for S3-compatible non-AWS endpoints
  AWS_S3_FORCE_PATH_STYLE optional, "true" to force path-style addressing
  S3_BUCKET               required
  TEMPLATER_URL           optional, default "http://localhost:8080/"

Usage:
  ./render-to-s3.py <key> [--template NAME] [--inputs FILE] [--expires-in SECONDS]
"""

import argparse
import json
import mimetypes
import os
from pathlib import Path

import boto3
import httpx
from botocore.config import Config

DEFAULT_TEMPLATE = "test.j2"
DEFAULT_INPUTS_FILE = Path(__file__).parent / "examples" / "render-inputs.json"


def _s3_client():
    path_style = os.environ.get("AWS_S3_FORCE_PATH_STYLE", "").lower() in (
        "1",
        "true",
        "yes",
    )
    return boto3.client(
        "s3",
        endpoint_url=os.environ.get("AWS_ENDPOINT_URL"),
        config=Config(s3={"addressing_style": "path" if path_style else "auto"}),
    )


def guess_content_type(filename: str) -> str:
    """Guess Content-Type from the output filename (the S3 key), so it
    matches what's signed into the presigned URL below."""
    return mimetypes.guess_type(filename)[0] or "text/plain"


def presigned_put_url(key: str, expires_in: int, content_type: str) -> str:
    return _s3_client().generate_presigned_url(
        "put_object",
        Params={
            "Bucket": os.environ["S3_BUCKET"],
            "Key": key,
            "ContentType": content_type,
        },
        ExpiresIn=expires_in,
    )


def presigned_get_url(
    key: str, expires_in: int, content_type: str | None = None
) -> str:
    """Presigned GET URL for opening `key` directly in a browser tab: forces
    inline display (via Content-Disposition) instead of a download."""
    return _s3_client().generate_presigned_url(
        "get_object",
        Params={
            "Bucket": os.environ["S3_BUCKET"],
            "Key": key,
            "ResponseContentDisposition": "inline",
            "ResponseContentType": content_type or guess_content_type(key),
        },
        ExpiresIn=expires_in,
    )


def render_to_s3(
    template: str,
    key: str,
    inputs: list,
    templater_url: str,
    expires_in: int = 3600,
) -> str:
    """Render `template` with `inputs` on the templater service, writing the
    result to a presigned PUT URL for `key`. Returns that output URL."""
    output_url = presigned_put_url(key, expires_in, guess_content_type(key))
    payload = {"template": template, "inputs": inputs, "output": output_url}
    response = httpx.post(templater_url, json=payload, timeout=30.0)
    response.raise_for_status()
    return output_url


def read_input(path: Path) -> dict:
    """Load the render job's `input` dict from a JSON file."""
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        raise ValueError(f"cannot read inputs from {path}: {e}") from e


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("key", help="S3 object key to PUT the rendered output to")
    parser.add_argument(
        "--template",
        default=DEFAULT_TEMPLATE,
        help="template name (default: %(default)s)",
    )
    parser.add_argument(
        "--inputs",
        type=Path,
        default=DEFAULT_INPUTS_FILE,
        help="JSON file with the render job's `inputs` list (default: %(default)s)",
    )
    parser.add_argument(
        "--expires-in",
        type=int,
        default=3600,
        help="presigned URL TTL in seconds (default: %(default)s)",
    )
    return parser.parse_args(argv)


def main() -> None:
    args = parse_args()
    templater_url = os.environ.get("TEMPLATER_URL", "http://localhost:8080/")
    try:
        inputs = [read_input(args.inputs)]
    except ValueError as e:
        raise SystemExit(e) from e

    render_to_s3(
        args.template,
        args.key,
        inputs,
        templater_url,
        args.expires_in,
    )
    print(presigned_get_url(args.key, args.expires_in))


if __name__ == "__main__":
    main()
