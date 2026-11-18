# Game and DLC updates

Open **Updates** in the launcher and choose **Check for updates**. xTux checks the installed package versions of games and DLCs against the package offered to the signed-in account. Each row shows the installed version, available version and any check error.

Choose **Update** to add an available update to Downloads. Updates use the existing installation folder and download changed files. They use the same pause, resume and cancel controls as other downloads. Cancelling an update keeps the installation folder. If files have already changed, finish or retry the update before playing; xTux blocks launches while that update is incomplete. Updates refuse to write while a game or affected add-on is in use.

Files removed from the new package are removed only if they belonged to the previous package. Other files added by the user are kept. Completion commits the new package metadata and clears the incomplete-update marker. A resumed update only trusts completed-file records from the exact same package.

Installations of the Windows Xbox app are updated in place as well. xTux replaces only the package header and the changed files; the Xbox app's own bookkeeping files (`.xvi`, `.xvs`, `.xct`, `.smd`) stay untouched, so after an update from Linux the Xbox app may check or download the game again the next time Windows runs.

If the Xbox app started an update and did not finish it (for example Windows was shut down during an automatic update), its package header already names the new version while many game files are still empty placeholders. xTux blocks launches of such an install and lists it under Updates with "The Windows Xbox app did not finish this update; xTux can finish it". Its update checks every installed file page by page against the hashes of the new package, keeps the files that already match and downloads the rest. Files of the package's unencrypted part (manifest, logos, configuration) carry no page hashes and are always downloaded; they are small. xTux then notes the finished update next to the install, so the stale state of the Xbox app no longer blocks launches until Windows changes it again.

`xtux-cli verify '/path/to/library/Game'` runs the same check against the installed package and lists the files it could not confirm: missing, different, or from the unencrypted part of the package, which has no page hashes.

Predownload-only packages and unsupported package formats are reported without an install action.

CLI equivalents:

```sh
xtux-cli downloads check
xtux-cli downloads update '/path/to/library/Game'
xtux-cli downloads list
```

Checks and installation are manual. Opening the Updates page does not start a download.
