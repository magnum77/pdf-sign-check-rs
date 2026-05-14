# pdf-sign-check-rs

`pdf-sign-check-rs` is a small background service that helps integrate
[Paperless-ngx](https://docs.paperless-ngx.com/) with PDF digital signature
verification.

It exposes a webhook, receives a Paperless document id, downloads the matching
PDF from Paperless, runs `pdfsig`, parses the signature information, and then
updates the Paperless document:

- adds the tag `Podpisany cyfrowo` by default,
- creates the tag first if it does not exist,
- adds a document note with signer, signing time, validation status, certificate
  status, signature type, and signed ranges,
- skips placeholder `pdfsig` entries that have no signer, no real timestamp,
  unknown algorithm/type, and `Signature has not yet been verified.`,
- stores a detailed debug JSON file for every webhook call.

The service is intended for Paperless workflow/webhook automation, but it can
also accept direct webhook calls from other systems.

## Features

- HTTP webhook endpoint, default: `POST /webhook`
- Health endpoint: `GET /healthz`
- Configurable bind address, default: `0.0.0.0:3000`
- Daily logs under `logs/`
- Per-request debug dumps under `debug/`
- Concurrent request handling through Tokio/Axum
- Temporary PDF storage with cleanup after every check
- `pdfsig` execution without crashing the service if `pdfsig` is missing
- Paperless API integration using either token auth or username/password token
  exchange
- Linux systemd user service and macOS LaunchAgent helper script

## How It Works

1. Paperless calls the webhook after a document is consumed or updated.
2. The webhook request is saved to `debug/<request-id>.json`.
3. The service extracts `document_id` from one of the supported request formats.
4. The service downloads the original PDF from Paperless:

   ```text
   /api/documents/{document_id}/download/?original=true
   ```

5. The PDF is written to a temporary file.
6. `pdfsig` is run against the temporary PDF.
7. Parsed signatures are filtered so invalid placeholder entries are not treated
   as real signatures.
8. If at least one real signature is found, the service:
   - ensures the Paperless tag exists,
   - applies the tag to the document,
   - adds a note with parsed signature details.
9. The temporary PDF file is removed.

## Supported Webhook Input Formats

The service accepts `document_id` from several places because webhook clients
often differ in how they send parameters.

### Form Body Without Content-Type

This is supported, even when the request does not include a `Content-Type`
header:

```text
document_id=6&doc_url=https://paperless.example.com/documents/6/
```

### application/x-www-form-urlencoded

```text
document_id=6&doc_url=https://paperless.example.com/documents/6/
```

### Query String

```text
POST /webhook?document_id=6&doc_url=https%3A%2F%2Fpaperless.example.com%2Fdocuments%2F6%2F
```

### JSON

```json
{
  "document_id": 6,
  "doc_url": "https://paperless.example.com/documents/6/"
}
```

Nested JSON fields are flattened in debug output and can also be detected:

```json
{
  "document": {
    "document_id": 6
  }
}
```

### Multipart Form Data

Multipart payloads are supported for both metadata fields and direct PDF uploads:

```text
document_id=6
file=@document.pdf
```

When a PDF file is included directly in the webhook request, the service can run
`pdfsig` on that file without downloading it from Paperless.

### Header Context

These headers are also checked:

- `x-paperless-document-id`
- `x-document-id`
- `x-paperless-document-url`
- `x-document-url`

## Debug Files

Every webhook call creates a JSON file in `debug/`. The most important sections
are:

- `ASK`: what the webhook received
- `RESPONSE`: what the service returned
- `context`: extracted document id and URL
- `pdf`: temporary PDF handling
- `pdfsig`: raw and parsed signature information
- `paperless`: Paperless API update status

`ASK.parameters` is split by source:

- `query`
- `json`
- `form_urlencoded`
- `multipart`
- `header_context`
- `combined`

This makes it easier to diagnose Paperless workflow/webhook payload issues.

Sensitive headers such as `Authorization`, `Cookie`, and `x-api-key` are redacted
in debug output.

## Requirements

- Rust toolchain
- `pdfsig`, provided by Poppler
- Paperless-ngx API access
- Optional but recommended on Linux: NSS tools for `pdfsig` certificate database
  initialization

On Debian/Ubuntu:

```bash
sudo apt update
sudo apt install poppler-utils libnss3-tools
```

If `pdfsig` prints NSS database warnings, initialize a local NSS database:

```bash
mkdir -p ~/.pki/nssdb
chmod 700 ~/.pki/nssdb
certutil -d sql:$HOME/.pki/nssdb -N --empty-password
```

## Installation

Clone and build:

```bash
git clone https://github.com/magnum77/pdf-sign-check-rs.git
cd pdf-sign-check-rs
cargo build --release
```

Create a local configuration file:

```bash
cp .env.example .env
```

Edit `.env`:

```env
PDF_SIGN_CHECK_BIND=0.0.0.0:3000
PDF_SIGN_CHECK_WEBHOOK_PATH=/webhook
PDF_SIGN_CHECK_DEBUG_DIR=debug
PDF_SIGN_CHECK_LOG_DIR=logs
PDF_SIGN_CHECK_TEMP_DIR=/tmp/pdf-sign-check-rs
PDF_SIGN_CHECK_MAX_BODY_BYTES=26214400

PAPERLESS_URL=https://paperless.example.com
PAPERLESS_TOKEN=
PAPERLESS_USERNAME=
PAPERLESS_PASSWORD=
PAPERLESS_TAG_NAME=Podpisany cyfrowo
PAPERLESS_API_VERSION=9
```

Use either:

- `PAPERLESS_TOKEN`, or
- `PAPERLESS_USERNAME` and `PAPERLESS_PASSWORD`

Do not commit `.env`.

## Running

Development:

```bash
cargo run
```

Production binary:

```bash
./target/release/pdf-sign-check-rs
```

Health check:

```bash
curl http://127.0.0.1:3000/healthz
```

Example webhook call:

```bash
curl -X POST \
  --data-binary 'document_id=6&doc_url=https://paperless.example.com/documents/6/' \
  http://127.0.0.1:3000/webhook
```

## Service Management

The helper script supports Linux and macOS:

```bash
scripts/service.sh install
scripts/service.sh status
scripts/service.sh restart
scripts/service.sh stop
scripts/service.sh start
scripts/service.sh uninstall
```

On Linux, it installs a user systemd service by default. To install as a system
service:

```bash
SYSTEM_SERVICE=1 sudo -E scripts/service.sh install
```

Environment overrides:

```bash
APP_NAME=pdf-sign-check-rs
PROJECT_DIR=/opt/pdf-sign-check-rs
BIN_PATH=/opt/pdf-sign-check-rs/target/release/pdf-sign-check-rs
ENV_FILE=/opt/pdf-sign-check-rs/.env
```

## Paperless Setup

Create a Paperless workflow or webhook that sends the consumed document id to
this service. A minimal request body is enough:

```text
document_id={{ document_id }}
```

If your workflow can include a URL, this is also useful for debugging:

```text
document_id={{ document_id }}&doc_url={{ document_url }}
```

The exact Paperless template variable names depend on the workflow/hook
configuration. Check the generated `debug/*.json` files if the service does not
extract `document_id`.

## Signature Notes

The Paperless note is generated from actionable signatures only. Placeholder
entries like this are ignored:

- empty signer name,
- empty distinguished name,
- `Jan 01 1970 00:00:00` signing time,
- `unknown` hash algorithm,
- `unknown` signature type,
- `Signature has not yet been verified.`

This avoids marking technical placeholder entries as real digital signatures.

## Development

Format and test:

```bash
cargo fmt --check
cargo test
```

Useful directories:

- `debug/`: request and processing dumps
- `logs/`: daily service logs
- `/tmp/pdf-sign-check-rs`: temporary PDF files

## Security Notes

- Keep `.env` private.
- Prefer `PAPERLESS_TOKEN` over username/password where possible.
- Restrict access to the webhook endpoint at the network/proxy level.
- Debug files may contain request payloads and document metadata. Treat `debug/`
  as sensitive operational data.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
