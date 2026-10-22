## Change Log

### Unreleased (external-storage-providers branch)

- External main drives and named folders using S3, native Backblaze B2, SFTP,
  FTPS, FTP, WebDAV, and SMB; protocol-first connection forms and saved-drive lists.
- Optional authenticated encryption of remote contents and names, with raw
  hexadecimal keys or Argon2id-derived passphrases and namespace recovery metadata.
- Destination existence/size checks after remote uploads, retained move sources
  on transfer failure, and virtual local/external copy/move destination browsing.
- Password-verified encrypted database export/restore with recovery phrases,
  import preview, transactional replacement, and session/public-link invalidation.
- Range-aware remote video streaming, including encrypted videos and public links;
  other remote responses use a plaintext cache expiring after 24 idle hours.
- Live byte-weighted copy/move progress, stage and filename reporting, and retained
  progress details on failures.
- MOV playback source-hint fix, localized media-load errors, player cleanup,
  and Safari caption support.
- Source-built Docker/Compose deployment, migration guidance, and packaged
  third-party license notices. These changes are not a tagged upstream release.

### v0.2.6

- Folder download

### v0.2.5

- File copy
- File move
- Drag and drop upload

### v0.2.4

- Reset password
- Update hidden files when renaming or deleting

This version uses directory `data/db` to store database files instead of `db`, which means:

- Docker command should change accordingly, see `README.md`
- Move `db` directory into `data` directory to keep previous data

### v0.2.3

- Allow guest users login
- Hide files from guest users

### v0.2.2

- File search
- Change modal style

### v0.2.1

- Context menu
- File rename
- File delete
- HTTPS via config file

### v0.2

- File upload
- Folder create
- Character encoding other than UTF
- Local IP address on other platforms

### v0.1.2

- Optionally use config file oasis.conf to specify the IP address and port number
- Prevent program crash when no local IP addresses are retrieved

### v0.1.1

- External media player support via the shared link
- File download
- Refactor range request handler
- Check app version against database version for update needs
- Bug fix: token refresh frequency, update modal overflow

### v0.1

- Initial version with basic functionalities.
