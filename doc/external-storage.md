# External storage

Oasis can use S3-compatible buckets, SFTP, FTPS, FTP, WebDAV and SMB without OS
mounts. The browser always talks to Oasis; credentials stay on the server.
Provider changes are read on each operation, including files added by other
clients to unencrypted storage. This is direct storage access, not a background
synchronization or local mirror.

## Server requirements

Install [rclone 1.71 or newer](https://rclone.org/install/) on the server PATH, put
it beside the Oasis executable, or set `OASIS_RCLONE` to its absolute path. The
Dockerfile installs rclone and CA certificates. No rclone configuration file is
needed: Oasis supplies each connection privately to the child process.
Source builds require Rust 1.85 or newer.

Oasis still needs writable local space for its SQLite database and temporary
processing. Allow space for two copies of the largest concurrent upload/download.
For a private CA, set `OASIS_STORAGE_CA_CERT` to a PEM CA bundle readable by Oasis.
TLS certificate checking is always enabled.

## Docker Compose

From the repository root, run:

```sh
docker compose -f compose.example.yaml up --build -d
docker compose -f compose.example.yaml logs -f oasis
```

The first command builds this checkout using `docker/Dockerfile.source`; no
prebuilt binary or old release archive is needed. Docker builds the Linux binary
for its own architecture. The separate `docker/Dockerfile` still packages an
already prepared Alpine release directory.

Open http://localhost:8000, finish setup with `/home/storage` as the local folder,
then configure external providers in Settings. Named volumes persist the database
(`oasis-data`) and local files (`oasis-files`). Temporary files use writable
container disk at `/opt/oasis/temp`, cleared by Oasis on startup. Do not mount
that directory directly: startup removes and recreates it. Budget space for two
copies of concurrent transfers. `docker compose -f compose.example.yaml down` keeps
these volumes; adding `--volumes` deletes them, including settings and local files.

Optionally copy `.env.example` to `.env` to change the host port or bind address,
or set `OASIS_STORAGE_KEY` to a strong passphrase of at least 12 characters or a securely generated 64-character hexadecimal key
and uncomment its environment entry in Compose. Leave that entry commented to
use provider keys saved in Settings. Keep the key stable and back it up
separately. The default host binding is loopback; set
`OASIS_BIND_ADDRESS=0.0.0.0` for LAN access and arrange HTTPS when needed.

For SFTP or a private CA, uncomment the read-only `./secrets` mount in the example
and create that directory with the required files. The container runs as UID/GID
10001: mounted files must be readable by that identity. In Settings, use container
paths such as `/run/oasis-secrets/id_ed25519` and
`/run/oasis-secrets/known_hosts`. For a private CA, also uncomment
`OASIS_STORAGE_CA_CERT` and the `environment:` heading. Provider hosts must be reachable from the container;
`localhost` refers to the Oasis container itself. No privileged mode or FUSE is
needed. `.env` and `secrets/` are excluded from Git and the image build context.

## Configure drives

After initial Oasis setup, open **Settings > External storage**, enable providers,
and choose a mode:

- **Main drive:** select one provider. Regular file operations use its root;
  the local file-storage directory is not accessed. Other configured providers
  are retained for switching modes.
- **Folders alongside local storage:** every configured provider appears at the
  root using its drive name. Names must be unique and cannot shadow local entries.
  Drive roots cannot be deleted, renamed or moved through file operations; edit
  their names or remove the connection in settings instead.

Select **Add provider**, choose a protocol, then **Continue** to open its connection
form. Configured storages appear in a compact list with their protocol and
encryption status; select **Edit** to open one form at a time. Drafts survive
switching editors and are marked **Unsaved changes** until saved. Enter connection
details, **Test connections**, then **Save storage settings**. These controls save
independently of the general site settings.
Saving checks connections and initializes new encrypted roots. Removing or
switching a connection never migrates or deletes its data. Use copy/move to
transfer files between local and external drives. Do not configure overlapping
remote roots as separate drives.

| Protocol | Connection details |
| --- | --- |
| S3 | `bucket/optional-prefix`, region, access key and secret; optional session token. Leave credentials blank for the server AWS credential chain. Set the endpoint for S3-compatible services. |
| SFTP | Host, optional port (default 22), username and password or server-side SSH private key path/agent. A trusted server-side `known_hosts` file is required. Roots beginning with `/` are absolute; other roots are relative to the remote account. |
| FTPS | Host, username/password, optional port. Explicit TLS is the default (21); implicit TLS defaults to 990. Trust the server certificate or configure a CA bundle. |
| FTP | Host, optional port (21), username/password. **Insecure:** credentials and traffic are unencrypted. Prefer SFTP or FTPS. File encryption does not protect FTP credentials. |
| WebDAV | Base HTTP(S) endpoint, optional username/password, and a folder relative to that endpoint. Prefer HTTPS. |
| SMB | Host, optional port (445), username/password, optional domain, and `share/optional-path`. |

Use dedicated roots with listing, reading, writing and deletion permissions. Oasis
uses portable file names and does not follow local symbolic links. It
preserves empty S3 folders using `.oasis-dir` markers. These markers are hidden
from users and excluded from archives. Provider-side quotas still apply.

## Encryption and recovery

Enable encryption for an **empty, dedicated remote root**. Supply a random
256-bit key as 64 hexadecimal characters, or a passphrase of at least 12 Unicode
characters (up to 1,024 UTF-8 bytes), in settings or `OASIS_STORAGE_KEY`.
Passphrases preserve case and all spaces exactly; whitespace-only values are
rejected. Exactly 64 hexadecimal characters always mean a raw key and remain
case-insensitive, preserving compatibility with existing encrypted files.
The environment value takes precedence for every encrypted provider. For example:

```sh
openssl rand -hex 32
export OASIS_STORAGE_KEY='<64 hexadecimal characters>'
```

Back up the key separately before uploading. **Changing or losing it makes
existing files unreadable.** Oasis refuses a different key or disabling encryption
for an initialized encrypted root. Key rotation requires copying plaintext through
Oasis to another empty encrypted provider with its new key, verifying the copy,
and then removing the old data. A global environment key must be unset to use
different saved keys for this migration. Existing plaintext data is never silently
encrypted in place.

Passphrases derive a 256-bit key using Argon2id v1.3 (64 MiB, three iterations,
one lane) with a fresh random 128-bit salt per provider namespace. The public
salt and derivation version are retained in the saved configuration and the
namespace marker; keep the marker when moving or reconnecting storage. The
marker also authenticates the derived key. Keys are prepared asynchronously
and cached only in memory, so normal file operations do not repeat Argon2id.
Encrypted database exports include the derivation metadata; environment secrets
must still be moved separately. Older Oasis versions cannot read new
passphrase-encrypted namespaces. Existing hexadecimal-key namespaces are unchanged.

Contents use versioned AES-256-GCM STREAM framing with independently derived
per-file keys (HKDF-SHA-256 with a random 256-bit salt), authenticated length,
ordered 64 KiB chunks and an authenticated final chunk. Names use deterministic
AES-256-SIV with a separate derived key and lowercase base32 encoding. This hides
names and extensions, including from case-insensitive providers; encrypted names
are limited to approximately 140 UTF-8 bytes. The provider can still see exact
plaintext file sizes (stored in the authenticated, unencrypted header), hierarchy,
name lengths, identical name patterns, timestamps and access patterns.

Downloads, previews, range responses, subtitles and ZIP archives are plaintext.
Files added directly to an encrypted root must use the Oasis format; ordinary
provider clients cannot read or edit encrypted files. Preserve the hidden
`.oasis-storage-v1` namespace marker, which verifies the key when reconnecting.

Configuration is administrator-only. Passwords, tokens and saved encryption keys
are omitted from API responses; blank secret fields retain saved values. Settings
are sealed with a key derived from the site secret before being stored in SQLite.
Back up the database and protect server access: this does not protect against
someone who controls the server or its complete database.

## Temporary files and failures

Uploads are assembled in private operation directories, encrypted if requested,
and removed after success, failure or cancellation. Individual consumed upload
slices are deleted during assembly. Uploads idle for an hour are reaped every
minute. Restarting Oasis clears staging left by a crash.

Remote downloads are fetched into private staging and, when needed, authenticated
and decrypted before serving. The response owns staging until it finishes or the
client disconnects. ZIP downloads fetch one file at a time and cancel outstanding
work when the response closes. There is no persistent remote-file cache. Downloads
need space for the entire file and start after retrieval/decryption; a range
request currently fetches and decrypts the whole remote file.

Every external upload, including internal markers, is followed by a fresh
destination metadata query. Success requires an existing file with the exact
uploaded byte count; encrypted uploads are checked against the ciphertext size.
This applies to all six protocols, browser uploads, copies, moves and renames,
without an additional setting or file download. Missing files, unknown sizes,
size mismatches and metadata query failures fail the operation. Matching size
does not detect corruption that preserves length and is not full checksum
verification. Rclone's existing transfer checks still apply.

A move deletes its source only after all destination transfers and their size
checks succeed. Browser upload staging is still cleaned up on failure. Failed
operations can leave completed destination files or provider-managed partial
uploads; atomic replacement depends on the protocol/server. No background cleanup
purges provider files automatically, since they are the persistent drive data.

## Validation

```sh
cd frontend
npm ci
npm run check
npm run build
cd ../backend
cargo test
cargo build
cd ..
python3 -m venv /tmp/oasis-storage-test-venv
/tmp/oasis-storage-test-venv/bin/pip install pyftpdlib pyOpenSSL
/tmp/oasis-storage-test-venv/bin/python scripts/test_external_storage.py --rclone /absolute/path/to/rclone
# Also test SMB using a disposable loopback-only Samba Docker container:
/tmp/oasis-storage-test-venv/bin/python scripts/test_external_storage.py --rclone /absolute/path/to/rclone --smb
```

The opt-in integration suite creates temporary local protocol servers, credentials
and storage, then removes them. It exercises encrypted uploads, listing, empty
files, plaintext downloads, byte ranges, sharing, subtitles, ZIPs, renames,
cross-drive transfers, deletion, staging cleanup, key-change guards, main-drive
operation without local storage, and settings persistence across restart. It also
injects destination metadata failures to confirm rejected uploads and retained
move sources, and checks that verification never downloads file contents.

Passphrase-specific recovery tests:

```sh
python3 scripts/test_storage_passphrases.py --rclone /absolute/path/to/rclone
python3 scripts/test_external_storage.py --rclone /absolute/path/to/rclone --protocols webdav --passphrase
python3 scripts/test_database_backup.py --rclone /absolute/path/to/rclone --passphrase
```
