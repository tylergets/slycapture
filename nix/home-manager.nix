{self}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.slycapture;
  yaml = pkgs.formats.yaml {};
  generatedConfig = yaml.generate "slycapture-config.yaml" cfg.settings;
in {
  options.programs.slycapture = {
    enable = lib.mkEnableOption "slycapture screen capture applet";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The slycapture package to run.";
    };

    settings = lib.mkOption {
      type = yaml.type;
      default = {};
      example = lib.literalExpression ''
        {
          recording = { output_dir = "~/Videos/slycapture"; format = "mp4"; framerate = 60; };
          audio.microphone = true;
          window.workspace = "rec";
        }
      '';
      description = "slycapture configuration, written as YAML (see README.md for keys).";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    systemd.user.services.slycapture = {
      Unit = {
        Description = "slycapture screen capture applet";
        After = ["graphical-session.target"];
        PartOf = ["graphical-session.target"];
      };

      Service = {
        ExecStart = "${lib.getExe cfg.package} --config ${generatedConfig} run";
        Restart = "on-failure";
        RestartSec = 2;
      };

      Install = {
        WantedBy = ["graphical-session.target"];
      };
    };
  };
}
