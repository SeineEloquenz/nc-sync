# nc-sync

A headless Nextcloud sync client. [rclone bisync](https://rclone.org/bisync/) does the
actual syncing; nc-sync decides when to run it and handles the account:

- **Login** through Nextcloud's browser flow (Login Flow v2), so SSO/OIDC works. The
  resulting app password is stored in the Secret Service keyring, never on disk.
- **Instant sync** in both directions: local changes are picked up with inotify, remote
  ones through the server's [notify_push](https://github.com/nextcloud/notify_push)
  websocket. A full sync also runs every few minutes as a fallback.
- **Desktop notifications** for conflicts, safety aborts and a revoked app password.

No Qt, no GUI.

## Behaviour

| Situation | What happens |
|---|---|
| First run, or the filters changed | Both sides are merged once (`--resync-mode newer`); nothing is deleted |
| A file changed on both sides | The newer version wins, the other is kept as `name.conflict1` |
| More than half the files would be deleted | Sync pauses; `nc-sync sync --force` continues |
| One side is completely empty | Sync refuses, even with `--force` |
| The app password was revoked | The keyring entry is removed and a login notification appears |
| The server has no notify_push | Remote changes are picked up by the periodic sync only |

Temporary, lock and old-client files (`.sync_*.db`, `.~lock.*`, `*.part`, …) never sync;
see `DEFAULT_EXCLUDES` in `src/config.rs`.

## Usage

```
nc-sync login            # opens the browser, stores an app password
nc-sync run              # sync continuously (what the service runs)
nc-sync sync [--resync | --force]
nc-sync logout           # revokes the app password on the server
```

The config lives in `~/.config/nc-sync/config.toml`:

```toml
server = "https://cloud.example.com"
local_dir = "~/Nextcloud"
remote_dir = "/"          # folder on the server
interval = 300            # seconds between full syncs
debounce = 3              # seconds of quiet before a sync starts
exclude = ["*.iso"]       # extra rclone filter patterns
rclone_args = []          # extra arguments for rclone bisync
```

## Home Manager

```nix
{
  inputs.nc-sync.url = "github:SeineEloquenz/nc-sync";

  # in the Home Manager modules
  imports = [ inputs.nc-sync.homeModules.default ];

  services.nc-sync = {
    enable = true;
    server = "https://cloud.example.com";
  };
}
```

This adds a `nc-sync` user service bound to the graphical session, which it needs for
the keyring and notifications.

## Server

Enable notify_push for instant remote changes. On NixOS:

```nix
services.nextcloud.notify_push.enable = true;
```

The push server talks to Nextcloud through its public URL, so that address must reach
Nextcloud directly and the push server's address must be in `trusted_proxies`.

## Development

```
nix develop
cargo test   # includes tests that run real rclone between two local folders
nix flake check
```

`nix flake check` also runs a NixOS VM test (`nix/tests/nextcloud.nix`) against a real
Nextcloud with notify_push: browser login (a script stands in for the browser), the
keyring, syncing in both directions, push-triggered downloads, conflicts, the delete
safety check, revocation with re-login, and logout. It always uses the newest Nextcloud
in nixpkgs.
