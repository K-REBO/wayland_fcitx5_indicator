{
  description = "IME mode indicator for fcitx5 on Wayland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        wayland_fcitx5_indicator = pkgs.rustPlatform.buildRustPackage {
          pname = "wayland_fcitx5_indicator";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            cairo
            glib
            wayland
            dbus
          ];

          meta = with pkgs.lib; {
            description = "IME mode indicator for fcitx5";
            homepage = "https://github.com/K-REBO/wayland_fcitx5_indicator";
            license = licenses.mit;
            platforms = platforms.linux;
          };
        };
      in
      {
        packages = {
          default = wayland_fcitx5_indicator;
          wayland_fcitx5_indicator = wayland_fcitx5_indicator;
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            rust-analyzer
            pkg-config
          ];

          buildInputs = with pkgs; [
            cairo
            cairo.dev
            glib
            glib.dev
            wayland
            wayland.dev
            dbus
            dbus.dev
          ];

          shellHook = ''
            export PKG_CONFIG_PATH="${pkgs.lib.makeSearchPath "lib/pkgconfig" [
              pkgs.cairo.dev
              pkgs.glib.dev
              pkgs.wayland.dev
              pkgs.dbus.dev
            ]}"
          '';
        };
      }
    ) // {
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.wayland-fcitx5-indicator;
        in
        {
          options.programs.wayland-fcitx5-indicator = {
            enable = lib.mkEnableOption "wayland fcitx5 indicator";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              description = "The wayland_fcitx5_indicator package to use";
            };

            settings = lib.mkOption {
              type = lib.types.nullOr lib.types.attrs;
              default = null;
              description = "Configuration settings in RON format";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            xdg.configFile."wayland_fcitx5_indicator/config.ron" = lib.mkIf (cfg.settings != null) {
              text = builtins.toJSON cfg.settings;
            };
          };
        };
    };
}
