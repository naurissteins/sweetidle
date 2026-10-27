{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.sweetidle;
  tomlFormat = pkgs.formats.toml {};
  configFile = tomlFormat.generate "sweetidle-config.toml" cfg.settings;
  # reject a bad config at build time instead of at login
  checkedConfig = pkgs.runCommand "sweetidle-config.toml" {} ''
    ${lib.getExe cfg.package} --check-config --config ${configFile}
    cp ${configFile} $out
  '';
in {
  options.services.sweetidle = {
    enable = lib.mkEnableOption "sweetidle, a Wayland idle daemon, as a systemd user service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix {};
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/package.nix { }";
      description = "The sweetidle package to use.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      example = lib.literalExpression ''
        {
          general.before_sleep_command = "veila lock --wait-ready";
          listener = [
            { timeout = 300; on_timeout = "veila lock --wait-ready"; }
            {
              timeout = 600;
              on_timeout = "sweets msg output '*' power off";
              on_resume = "sweets msg output '*' power on";
            }
          ];
        }
      '';
      description = ''
        sweetidle configuration, written to /etc/sweetidle/config.toml and
        validated at build time. When set, the service uses this file instead
        of ~/.config/sweetidle/config.toml. Leave empty to manage that file
        yourself. See the README for available options.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
    systemd.packages = [cfg.package];

    environment.etc."sweetidle/config.toml" = lib.mkIf (cfg.settings != {}) {
      source = checkedConfig;
    };

    systemd.user.services.sweetidle = {
      wantedBy = ["graphical-session.target"];
      # hooks run through `sh -c` and need the user's PATH, not the minimal unit default
      enableDefaultPath = false;
      # the /etc path stays stable, so `systemctl --user reload sweetidle` picks up rebuilds
      serviceConfig.ExecStart = lib.mkIf (cfg.settings != {}) [
        ""
        "${lib.getExe cfg.package} --config /etc/sweetidle/config.toml"
      ];
    };
  };
}
