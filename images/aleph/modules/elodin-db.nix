{
  pkgs,
  lib,
  config,
  ...
}: let
  cfg = config.services.metor-db;
in {
  options.services.metor-db = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Whether to enable the metor-db service.
      '';
    };
    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Whether to automatically open the specified ports in the firewall.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.metor-db = with pkgs; {
      wantedBy = ["multi-user.target"];
      after = ["network.target"];
      description = "start metor-db";
      serviceConfig = {
        Type = "exec";
        User = "root";
        ExecStart = "${metor-db}/bin/metor-db run [::]:2240 --http-addr [::]:2248 /db";
        KillSignal = "SIGINT";
        Environment = "RUST_LOG=info";
      };
    };
    environment.systemPackages = [pkgs.metor-db];
    networking.firewall.allowedTCPPorts = lib.optionals cfg.openFirewall [2240 2248];
  };
}
