## OASIS

[![Build](https://github.com/machengim/oasis/actions/workflows/build_release.yml/badge.svg)](https://github.com/machengim/oasis/actions/workflows/build_release.yml) [![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/machengim/oasis/blob/main/LICENSE-MIT) [![GitHub release (latest SemVer)](https://img.shields.io/github/v/release/machengim/oasis)](https://github.com/machengim/oasis/releases) [![Docker](https://img.shields.io/badge/docker-v0.2.6-orange)](https://hub.docker.com/r/machengim/oasis)

[中文 README](https://github.com/machengim/oasis/blob/main/README_cn.md)

A self-hosted file server.

![](https://github.com/machengim/oasis/blob/main/doc/demo.png?raw=true)

### Install

1. Download from the [release](https://github.com/machengim/oasis/releases) page
2. Uncompress
3. Grant execute permission to `oasis` file if running in Linux or MacOS
4. (Optional) Config server IP and port number in `oasis.conf` file
5. Run `oasis` or `oasis.exe`
6. Visit the server's IP address in your favorite browser

### Docker

To build and run this checkout with external storage support:

```sh
docker compose -f compose.example.yaml up --build -d
```

Open http://localhost:8000 and choose `/home/storage` during initial setup.
Configure providers in **Settings → External storage**. The example persists
settings and local files, and includes rclone. See [Compose setup](doc/external-storage.md#docker-compose)
for encryption, certificates, and network configuration.

The published image below may not yet include changes from this branch:

https://hub.docker.com/r/machengim/oasis

```
docker run --name oasis -t -d \
-v <data>:/opt/oasis/data \
-v <storage>:/home/storage \
-p <port>:8000 machengim/oasis
```

### Build

- Node 14+
- Rust 1.85+

```
cd path/to/oasis
node build.js
```

### Features

- User authentication
- File preview/download/upload/Search
- Media file play list
- File external link
- I18n (English, Chinese)
- External storage: S3, SFTP, FTPS, FTP, WebDAV and SMB, with optional AES-256 encryption

### External storage

Configure providers in **Settings → External storage**. Use an external provider
as the main drive, or show named external folders beside local files. Install
rclone 1.71+ on the server (included by the Dockerfile).

See [external storage configuration](doc/external-storage.md) for protocol setup,
encryption keys, deployment requirements and temporary-file handling.

### File format support

- Text
- Image (browser support)
- Audio (browser support)
- Video (browser support)
- Subtitle (`srt` / `vtt` format, supported in Chrome, Firefox and Edge by now)
- PDF (supported by pdf.js)

### Tech stack

- [Svelte](https://svelte.dev)
- [Rocket](https://rocket.rs)
- [Tailwind](https://tailwindcss.com)

### Credits

- [Pdf.js](https://mozilla.github.io/pdf.js)
- [Plyr](https://plyr.io)
