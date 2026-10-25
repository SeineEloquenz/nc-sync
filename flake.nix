{
  description = "nc-sync: headless Nextcloud sync built on rclone bisync and notify_push";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          nc-sync = pkgs.callPackage ./nix/package.nix { src = ./.; };
        in
        {
          inherit nc-sync;
          default = nc-sync;
        }
      );

      homeModules.default = import ./nix/hm-module.nix self;

      checks = forAllSystems (
        pkgs:
        let
          inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) nc-sync;
        in
        {
          inherit nc-sync;
          nextcloud = import ./nix/tests/nextcloud.nix { inherit pkgs nc-sync; };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            rclone
          ];
        };
      });
    };
}
