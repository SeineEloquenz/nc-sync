# End-to-end test against a real Nextcloud with notify_push: browser login,
# keyring, WebDAV sync in both directions, push-triggered downloads, conflicts,
# the delete safety check, revocation and re-login, and logout.
{
  pkgs,
  nc-sync,
}:

let
  inherit (pkgs) lib;

  # the newest nextcloudNN in nixpkgs; the removed ones are throwing aliases with lower numbers
  nextcloud =
    let
      version = name: lib.toInt (lib.removePrefix "nextcloud" name);
      names = lib.filter (name: builtins.match "nextcloud[0-9]+" name != null) (lib.attrNames pkgs);
    in
    pkgs.${lib.last (lib.sort (a: b: version a < version b) names)};

  user = "alice";
  # the password_policy app rejects simple passwords
  password = "Q8w!zr4Tk2#pLx9e";

  grantLogin = pkgs.writers.writePython3 "grant-login" {
    flakeIgnore = [ "E501" ];
  } (builtins.readFile ./grant-login.py);

  # what the user's session provides, for test commands and the user services alike
  sessionEnv = {
    XDG_RUNTIME_DIR = "/run/user/1000";
    DBUS_SESSION_BUS_ADDRESS = "unix:path=/run/user/1000/bus";
    NC_SYNC_CONFIG = "/etc/nc-sync.toml";
    BROWSER = "${grantLogin}";
    GRANT_USER = user;
    GRANT_PASSWORD = password;
  };

  # runs a command as the user, inside their session bus
  asUser = pkgs.writeShellScript "as-user" ''
    exec ${pkgs.util-linux}/bin/runuser -u ${user} -- env \
      ${lib.concatStringsSep " " (lib.mapAttrsToList (k: v: "${k}=${lib.escapeShellArg v}") sessionEnv)} \
      "$@"
  '';
in
pkgs.testers.runNixOSTest {
  name = "nc-sync-nextcloud";

  nodes.nextcloud = {
    virtualisation.memorySize = 2048;
    networking.firewall.allowedTCPPorts = [ 80 ];

    services.nextcloud = {
      enable = true;
      package = nextcloud;
      hostName = "nextcloud";
      https = false;
      database.createLocally = true;
      config.dbtype = "pgsql";
      configureRedis = true;
      notify_push = {
        enable = true;
        # the push server reaches Nextcloud through localhost, as on a real host
        bendDomainToLocalhost = true;
        logLevel = "debug";
      };
      settings = {
        trusted_proxies = [ "::1" ];
        # no sample files, so the sync only sees what the test creates
        skeletondirectory = "";
      };
    };
  };

  nodes.client = {
    virtualisation.memorySize = 1024;

    users.users.${user} = {
      isNormalUser = true;
      uid = 1000;
      # starts the user manager and session bus at boot, without a login
      linger = true;
    };

    environment.systemPackages = [
      nc-sync
      pkgs.libsecret
      pkgs.curl
    ];

    environment.etc."nc-sync.toml".text = ''
      server = "http://nextcloud"
      local_dir = "~/Nextcloud"
      # long enough that only notify_push can explain a quick download
      interval = 3600
      debounce = 1
    '';

    # an unlocked login keyring, as after a desktop login. oo7-daemon rather than
    # gnome-keyring: started headless, the latter races and aborts in OpenSession.
    # oo7-daemon only unlocks a login keyring that exists, and oo7-cli needs a running
    # Secret Service even to write a keyring file, so a throwaway daemon with its own
    # data dir serves the CLI while it creates the real keyring.
    systemd.user.services.keyring = {
      wantedBy = [ "default.target" ];
      environment = sessionEnv;
      path = [
        pkgs.systemd
        pkgs.coreutils
      ];
      script = ''
        keyring="$HOME/.local/share/keyrings/v1/login.keyring"
        if [ ! -e "$keyring" ]; then
          XDG_DATA_HOME="$(mktemp -d)" ${pkgs.oo7-server}/libexec/oo7-daemon &
          throwaway=$!
          until busctl --user status org.freedesktop.secrets >/dev/null 2>&1; do sleep 0.1; done
          mkdir -p "$(dirname "$keyring")"
          printf 'seed' | ${pkgs.oo7}/bin/oo7-cli --keyring "$keyring" --secret keyring-password \
            store 'test seed' purpose=seed
          kill "$throwaway"
          wait "$throwaway" || true
        fi
        printf 'keyring-password' | exec ${pkgs.oo7-server}/libexec/oo7-daemon --login
      '';
    };

    systemd.user.services.nc-sync = {
      environment = sessionEnv // {
        RUST_LOG = "nc_sync=debug,info";
      };
      serviceConfig.ExecStart = "${nc-sync}/bin/nc-sync run";
    };
  };

  testScript = ''
    import json
    from datetime import timedelta

    USER_CMD = "${asUser}"
    DAV = "http://nextcloud/remote.php/dav/files/${user}"
    AUTH = "-u ${lib.escapeShellArg "${user}:${password}"}"
    LOGIN_URL = "http://nextcloud/login/v2/flow/"

    def as_user(cmd, timeout=120):
        return f"timeout {timeout} {USER_CMD} {cmd}"

    def wait_for(condition, seconds=60):
        retry(lambda _: condition(), timeout=timedelta(seconds=seconds))

    def remote_put(path, content):
        # from a file, not stdin: Nextcloud stores chunked uploads without a length as empty
        client.succeed(f"printf '%s\\n' '{content}' > /tmp/upload && curl -sSf {AUTH} -T /tmp/upload {DAV}/{path}")
        assert remote_get(path) == content, f"upload of {path} did not stick"

    def remote_get(path):
        status, content = client.execute(f"curl -sSf {AUTH} {DAV}/{path}")
        return content.strip() if status == 0 else None

    def remote_missing(path):
        code = client.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {AUTH} {DAV}/{path}")
        return code.strip() == "404"

    def local(path):
        return f"/home/${user}/Nextcloud/{path}"

    def local_write(path, content):
        client.succeed(as_user(f"sh -c 'mkdir -p ~/Nextcloud && echo {content} > {local(path)}'"))

    def local_get(path):
        status, content = client.execute(f"cat {local(path)}")
        return content.strip() if status == 0 else None

    def journal():
        return client.succeed("journalctl _SYSTEMD_USER_UNIT=nc-sync.service -o cat")

    def tokens():
        return json.loads(nextcloud.succeed("nextcloud-occ user:auth-tokens:list ${user} --output=json"))

    def keyring_has_password():
        # secret-tool search exits 0 without matches too, so look at what it found
        found = client.succeed(as_user("secret-tool search --all application nc-sync 2>&1"))
        return "attribute.login = ${user}" in found

    def user_systemctl(args):
        client.succeed(f"systemctl --user -M ${user}@ {args}")

    start_all()

    nextcloud.wait_for_unit("multi-user.target")
    nextcloud.wait_for_unit("nextcloud-notify_push.service")
    nextcloud.succeed("NC_PASS=${lib.escapeShellArg password} nextcloud-occ user:add ${user} --password-from-env")

    client.wait_for_unit("multi-user.target")
    client.wait_for_unit("user@1000.service")
    client.wait_until_succeeds(as_user("busctl --user status org.freedesktop.secrets"), timeout=timedelta(seconds=60))

    with subtest("login through the browser flow stores an app password"):
        status, out = client.execute(as_user("env RUST_LOG=debug nc-sync login 2>&1"))
        if status != 0:
            client.execute("cat /tmp/grant-login.log >&2")
        assert status == 0, f"login failed with {status}: {out}"
        assert "Logged in as ${user}" in out, out
        assert keyring_has_password()
        names = [t["name"] for t in tokens()]
        assert any(n.startswith("nc-sync (client)") for n in names), names

    with subtest("first sync merges both sides"):
        remote_put("from-server.txt", "server")
        local_write("from-client.txt", "client")
        local_write(".owncloudsync.log", "junk")
        client.succeed(as_user("nc-sync sync"))
        assert local_get("from-server.txt") == "server"
        assert remote_get("from-client.txt") == "client"
        assert remote_missing(".owncloudsync.log"), "excluded files stay local"

    with subtest("daemon connects to notify_push"):
        user_systemctl("start nc-sync.service")
        wait_for(lambda: "listening for remote changes" in journal())

    with subtest("local changes are uploaded"):
        local_write("new-local.txt", "uploaded")
        wait_for(lambda: remote_get("new-local.txt") == "uploaded")

    with subtest("remote changes arrive through notify_push"):
        remote_put("pushed.txt", "pushed")
        wait_for(lambda: local_get("pushed.txt") == "pushed")
        assert "triggered by Remote" in journal()

    with subtest("local deletions propagate"):
        client.succeed(f"rm {local('new-local.txt')}")
        wait_for(lambda: remote_missing("new-local.txt"))

    user_systemctl("stop nc-sync.service")

    with subtest("conflicts keep the newer version and the other as a copy"):
        local_write("from-server.txt", "older-local")
        client.succeed(f"touch -d '-2 min' {local('from-server.txt')}")
        remote_put("from-server.txt", "newer-remote")
        client.succeed(as_user("nc-sync sync"))
        assert local_get("from-server.txt") == "newer-remote"
        assert local_get("from-server.txt.conflict1") == "older-local"
        assert remote_get("from-server.txt.conflict1") == "older-local"
        out = client.succeed(as_user("nc-sync sync 2>&1"))
        assert "in sync (0 conflicts)" in out, f"a resolved conflict is reported again: {out}"

    with subtest("mass deletion pauses until forced"):
        client.succeed(as_user("sh -c 'mkdir -p ~/Nextcloud/bulk && for i in $(seq 1 20); do echo $i > ~/Nextcloud/bulk/$i.txt; done'"))
        client.succeed(as_user("nc-sync sync"))
        assert remote_get("bulk/20.txt") == "20"
        client.succeed(f"rm -r {local('bulk')}")
        out = client.fail(as_user("nc-sync sync 2>&1"))
        assert "too many deletes" in out and "--force" in out, out
        assert remote_get("bulk/20.txt") == "20", "nothing was deleted on the server"
        client.succeed(as_user("nc-sync sync --force"))
        assert remote_missing("bulk/20.txt")

    with subtest("a revoked app password leads to a new login"):
        user_systemctl("start nc-sync.service")
        wait_for(lambda: journal().count("listening for remote changes") == 2)
        for token in tokens():
            if token["name"].startswith("nc-sync"):
                nextcloud.succeed(f"nextcloud-occ user:auth-tokens:delete ${user} {token['id']}")

        # Nextcloud caches validated tokens briefly, so keep changing files until a
        # sync runs into the revocation
        def poke_and_check():
            local_write("poke.txt", client.succeed("date +%s%N").strip())
            return LOGIN_URL in journal()

        retry(lambda _: poke_and_check(), timeout=timedelta(seconds=120))
        local_write("after-revoke.txt", "after-revoke")
        assert not keyring_has_password()

        # the user clicks the notification; there is no notification daemon, so grant directly
        url = [word for word in journal().split() if word.startswith(LOGIN_URL)][-1]
        client.succeed(as_user(f"${grantLogin} {url}"))
        wait_for(lambda: remote_get("after-revoke.txt") == "after-revoke", seconds=90)
        assert keyring_has_password()

    user_systemctl("stop nc-sync.service")

    with subtest("logout revokes the app password and clears the keyring"):
        client.succeed(as_user("nc-sync logout"))
        assert not keyring_has_password()
        names = [t["name"] for t in tokens()]
        assert not any(n.startswith("nc-sync") for n in names), names
        out = client.fail(as_user("nc-sync sync 2>&1"))
        assert "not logged in" in out, out
  '';
}
