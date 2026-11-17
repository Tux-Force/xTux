# Game and DLC updates

Open **Updates** in the launcher and choose **Check for updates**. xTux checks the installed package versions of games and DLCs against the package offered to the signed-in account. Each row shows the installed version, available version and any check error.

Choose **Update** to add an available update to Downloads. Updates use the existing installation folder and download changed files. They use the same pause, resume and cancel controls as other downloads. Cancelling an update keeps the installation folder. If files have already changed, finish or retry the update before playing; xTux blocks launches while that update is incomplete. Updates refuse to write while a game or affected add-on is in use.

Files removed from the new package are removed only if they belonged to the previous package. Other files added by the user are kept. Completion commits the new package metadata and clears the incomplete-update marker. A resumed update only trusts completed-file records from the exact same package.

Installations managed by the Windows Xbox app can be checked, but their updates must be performed in Windows. xTux does not rewrite the Xbox app's bookkeeping files. Predownload-only packages and unsupported package formats are reported without an install action.

CLI equivalents:

```sh
xtux-cli downloads check
xtux-cli downloads update '/path/to/library/Game'
xtux-cli downloads list
```

Checks and installation are manual. Opening the Updates page does not start a download.
