# Automatic updates

Release builds check GitHub Releases shortly after launch and then once per
24 hours. Updates download while you use IRC and install on the next actual
exit. Closing or minimizing to the Windows tray keeps the application running
and does not install an update.

Settings → About provides **Check for updates**, download progress,
**Download update**, **Restart to update**, and the **Automatic updates** switch.
Manual checks bypass the daily interval. Disabling automatic updates also
disables installation on exit; an explicitly requested restart can still install
an update. Background checks never interrupt a connection or restart the app.

Restarting to update reconnects the active server, rejoins actual channel
memberships with their keys, reopens query tabs, and restores the selected tab,
away state, and unsent text. A disconnected session stays disconnected. The
temporary session snapshot is private and deleted after successful startup.
Chat history uses the existing local logs and IRCv3 history mechanisms.

Updates support the six existing release assets, chosen for the executable's
architecture. Renaming a downloaded executable is supported. Development builds
can check releases but cannot replace themselves. Unwritable installation
directories require a manual release download; the updater does not elevate.
macOS releases remain standalone universal binaries, not application bundles.

## Installation and recovery

The download must match both GitHub's asset digest and the release checksum
manifest. Windows also verifies both Authenticode signatures: Fara Technologies
LLC followed by Mike Fara. A downloaded candidate is never installed partially.
The download-time check requires certificate revocation data. The re-checks at
exit and in the install helper repeat the signature and publisher checks and
tolerate only revocation data being unavailable (for example, the machine went
offline); a revoked certificate or any other trust failure still blocks the
update, and the candidate's hash stays pinned by the journal.

Each attempt stages files in a private `.linefeed-update-<attempt>` directory
beside the executable, on the same filesystem. An independent copy of the old
executable performs replacement after graceful shutdown and after all instances
using that installation exit. Unix uses rename over the original; Windows uses
`ReplaceFileW` with a backup and recovery for partial native failures.

A journal in the user's local data directory, under
`linefeed/updates/<installation-id>/transaction.json`, records the candidate and
original hashes, paths, attempt, and phase. Interrupted operations are reconciled
against actual files on subsequent startup. Backups remain until the new GUI
starts successfully. Explicit restart waits up to 60 seconds for startup and
restores the old executable if startup fails. Failed versions are not retried
automatically; a manual restart may retry them.

A journal left behind by a manual or package-manager upgrade, or recorded for
paths that have since moved, holds nothing to recover once its update is
`Staged`, `Healthy`, or rolled back, and is discarded on the next launch. If
the executable instead matches neither version of an interrupted (`Prepared` or
`Replaced`) update, or the journal is unreadable, startup never overwrites
anything automatically: a normal launch still opens the app, preserves the
recovery files, and shows the issue in About with automatic installation
disabled. After installing a release yourself, **Reset update state** in
Settings → About deletes those files (the journal, its snapshot, and the staging
directory with any downloaded update and the backup of the previous version).
Restart Linefeed afterwards to re-enable updates.

Other trouble starting the updater (an unusable update data directory, or
another instance holding the update lock for more than about five seconds) is
handled the same way: Linefeed opens after a short wait with installation
disabled and the reason in About. It still holds the shared run lock where it
can, so another instance's update cannot replace its executable under it. A
restart-to-update launch that fails these checks is rolled back instead.

If another instance is still running when an update is due, the update stays
staged for the next exit and the application is not reopened. If the downloaded
update fails its validation run, an explicit restart reopens the previous
version with its session.

A download claims its staging directory with a locked `owner.lock`. At startup,
staging directories whose claim is no longer held (an exit or crash during a
download) are renamed to `.deleting` and removed; interrupted removals are
finished on a later launch. Directories without a claim, which includes
filesystems that cannot lock files, are never removed automatically; delete a
stray `.linefeed-update-*` folder by hand when no Linefeed is running.

Installation on ordinary exit validates the new executable without reopening
the GUI. GUI initialization is confirmed on the next launch. A failure before
the executable can run at all cannot be recovered by its own startup code. If
manual recovery is needed, close all Linefeed instances, copy
`.linefeed-update-<attempt>/backup` over the original executable, and launch it.
Keep the journal and helper until recovery completes. Atomic replacement requires
a local filesystem supporting same-volume replacement; network filesystems and
abrupt power loss can provide weaker guarantees.

## Publishing a release

Existing users need one manual upgrade to the first updater-capable release.
Future releases must finish assembling before they become public:

1. Bump the package version, commit, and push its matching `vX.Y.Z` tag.
   The release workflow creates a draft, tests the updater, builds Linux
   x64/ARM64 (on ubuntu-22.04, for a low glibc and OpenSSL 3 floor) and macOS,
   validates their embedded versions, and attaches their binaries.
2. Build Windows ARM64/x64/x86 locally and upload these three assets to that
   draft with `gh release upload`.
3. After all six assets are present, dispatch **Sign Release Binaries** for
   that tag. It signs all Windows executables, regenerates `SHA256SUMS` over
   the final six files, and uploads the signed bytes and checksums to the draft.
4. Dispatch **Publish verified release**. It verifies every checksum against
   GitHub's asset digest, both Windows publishers, Windows version resources,
   and tag/version agreement before publishing the draft as the latest release.

Build, signing, and publication workflows share a tag-specific concurrency
group. Dispatch each stage after the previous stage completes. These workflows
refuse to replace published assets; corrections require a new version.

The updater uses public HTTPS requests and does not require GitHub authentication.
Offline or rate-limited automatic attempts count toward the daily interval;
**Check for updates** remains available for an immediate retry.
