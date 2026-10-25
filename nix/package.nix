{
  lib,
  rustPlatform,
  makeWrapper,
  rclone,
  xdg-utils,
  src,
}:

let
  cargo = lib.importTOML "${src}/Cargo.toml";
in
rustPlatform.buildRustPackage {
  pname = cargo.package.name;
  inherit (cargo.package) version;

  # only what cargo builds from, so docs and Nix changes don't rebuild or retest
  src = lib.fileset.toSource {
    root = src;
    fileset = lib.fileset.unions [
      (src + "/Cargo.toml")
      (src + "/Cargo.lock")
      (src + "/src")
    ];
  };

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [ makeWrapper ];

  # the integration tests run real rclone between two local folders
  nativeCheckInputs = [ rclone ];

  # rclone does the syncing, xdg-open shows the login page
  postInstall = ''
    wrapProgram $out/bin/nc-sync \
      --prefix PATH : ${
        lib.makeBinPath [
          rclone
          xdg-utils
        ]
      }
  '';

  meta = {
    description = "Headless Nextcloud sync built on rclone bisync and notify_push";
    mainProgram = "nc-sync";
    platforms = lib.platforms.linux;
  };
}
