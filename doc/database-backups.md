# Database backup and restore

Open **Settings → Database backup and restore** as an administrator. Both export
and import require your current administrator password. A backup contains
accounts (including password hashes), site settings, file visibility rules, and
external storage configuration with saved credentials and encryption keys.

**File contents are not included.** Oasis lists files directly from the configured
folder or provider; there is no file-registration database to rebuild. Files
copied onto the server become visible when that folder is selected and Oasis has
the necessary filesystem permissions. Finish transfers before copying files for
an installation migration.

## Export

1. Select **Export database**, enter your administrator password, and generate
   a recovery phrase.
2. Save the numbered 12 English words in order. **Copy phrase** copies the whole
   phrase; **Download phrase.txt** saves a separate text file. If clipboard access
   fails, select and copy the displayed text manually.
3. Continue to word verification. Oasis hides the phrase and asks for three
   randomly selected word positions. Five incorrect submissions invalidate the
   export; start a new export to generate a new phrase.
4. Download the encrypted `.oasis-backup` file. Keep the recovery phrase separately
   from the backup. Matching identifiers in the filenames help pair them.

Each export has a different phrase. Losing it makes that backup unreadable; the
administrator password cannot replace it. If the original installation is still
available, create another export.

## Restore onto another installation

1. Install this version of Oasis and complete initial setup with a temporary
   administrator. Copy local files separately into the destination storage folder,
   or leave existing remote files on their provider.
2. Move required environment encryption keys, SSH keys, `known_hosts`, certificates,
   and server/reverse-proxy configuration separately. They are not in the backup.
3. Select **Import database**, choose the backup, enter all 12 recovery words and
   the **destination** administrator's password, and preview the import.
4. Review the source site and record counts. Select the destination's local folder;
   its current folder is the default, rather than the source server's old path.
5. Confirm replacement and re-enter the destination administrator password.

Restore replaces database state immediately without a container restart. It waits
for current request handlers and rejects active uploads or background copy/move
tasks. The database changes use one transaction: failed validation or SQL writes
leave current state unchanged. Local and remote files are never moved, deleted,
or uploaded by restore. Remote connections are not contacted during validation;
restore checks structure, local paths, and original active encryption keys.

After success, **all old login sessions and shared links are invalidated**, and
you sign in using an account from the backup. Password-reset requests are cleared.
Saved provider settings are resealed under the new site secret. A confirmed
restore continues if the browser disconnects, so reconnect and check the login
page before retrying an interrupted restore.

## Format and limits

Backups are versioned, encrypted logical snapshots, not raw SQLite or SQL files.
The v1 envelope uses `OASISDB1`, a random 128-bit salt, Argon2id v1.3 (64 MiB,
three passes, one lane), and the existing authenticated AES-256-GCM STREAM framing
with a fresh per-file salt. Recovery phrases use 128 bits of operating-system
randomness encoded as a 12-word English BIP39 mnemonic. Snapshots are read within
a SQLite transaction. Imported data is validated and inserted using bound values;
SQL, schema changes, reset requests, and active tasks are never imported.

Archives and decoded payloads are limited to 64 MiB. Pending operations expire
after 10 minutes; the server reaps expired jobs every minute. Each administrator
can have one pending job, with at most four server-wide. Only one expensive
backup operation runs at a time. Temporary files are private and removed after
download completion/disconnect, cancellation, failure, expiry, or successful
restore; startup removes crash leftovers. Secrets are not logged or placed in
URLs or browser storage, and all backup API responses use `no-store`.

## Validation

After building backend and frontend, run:

```sh
python3 scripts/test_database_backup.py --rclone /absolute/path/to/rclone
```

The test starts two disposable installations and a local encrypted WebDAV drive.
It checks password verification, recovery-word confirmation, ownership,
malformed/oversized archives, cancellation, source-to-destination migration,
rollback, existing-file discovery, visibility rules, stored keys, session/link
invalidation, and restart persistence. Add `--keep-alive` for Safari testing.
