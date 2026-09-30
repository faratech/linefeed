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

If a manual binary replacement or damaged journal prevents recovery, a normal
launch still opens the app, preserves recovery files, and shows the issue in
About with automatic installation disabled.

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
   The release workflow creates a draft, tests the updater, builds Linux x64
   and macOS, validates their embedded versions, and attaches their binaries.
2. Build Windows ARM64/x64/x86 and Linux ARM64 locally. Validate the Linux
   binary with `--internal-update-probe X.Y.Z`, and upload these four assets
   to that draft with `gh release upload`.
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
