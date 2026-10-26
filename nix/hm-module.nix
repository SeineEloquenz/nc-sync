self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.nc-sync;
  toml = pkgs.formats.toml { };

  # unset options are left out, so nc-sync's own defaults apply
  settings = lib.filterAttrs (_: value: value != null) {
    server = cfg.server;
    local_dir = cfg.localDir;
    remote_dir = cfg.remoteDir;
    interval = cfg.interval;
    debounce = cfg.debounce;
    exclude = cfg.exclude;
    rclone_args = cfg.rcloneArgs;
    tray = cfg.tray;
  };
in
{
  options.services.nc-sync = {
    enable = lib.mkEnableOption "nc-sync, a headless Nextcloud sync client";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.nc-sync;
      defaultText = lib.literalExpression "nc-sync.packages.\${system}.nc-sync";
      description = "The nc-sync package to use.";
    };

    server = lib.mkOption {
      type = lib.types.str;
      example = "https://cloud.example.com";
      description = "Base URL of the Nextcloud server.";
    };

    localDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/Nextcloud";
      defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/Nextcloud"'';
      description = "Local folder kept in sync with the server.";
    };

    remoteDir = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/Documents";
      description = "Folder on the server to sync, relative to the user's files. All of them if unset.";
    };

    interval = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      example = 600;
      description = ''
        Seconds without any change before a full sync runs anyway, as a fallback for
        missed change events. 300 if unset.
      '';
    };

    debounce = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      example = 10;
      description = "Seconds without further changes before a sync starts. 3 if unset.";
    };

    exclude = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "*.iso" "node_modules/**" ];
      description = ''
        Extra rclone filter patterns that never sync, on top of the built-in
        excludes for temporary, lock and old-client files.
        Changing them makes the next sync merge both sides once.
      '';
    };

    rcloneArgs = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "--bwlimit" "10M" ];
      description = "Extra arguments passed to `rclone bisync`.";
    };

    tray = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      example = false;
      description = ''
        Whether to show a status icon in the system tray. On if unset. GNOME needs the
        AppIndicator extension to display it.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."nc-sync/config.toml".source = toml.generate "nc-sync.toml" settings;

    systemd.user.services.nc-sync = {
      Unit = {
        Description = "Nextcloud file sync";
        # needs the keyring and the notification daemon of the session
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
        X-Restart-Triggers = [ config.xdg.configFile."nc-sync/config.toml".source ];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "on-failure";
        RestartSec = 30;
        # rclone finishes or rolls back its current transfer on SIGTERM
        TimeoutStopSec = 60;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
