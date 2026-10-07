{
  description = "sweetsidle development environment";

  inputs.nixpkgs.url = "https://channels.nixos.org/nixos-26.05/nixexprs.tar.zst";

  outputs = {nixpkgs, ...}: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forEachSystem = nixpkgs.lib.genAttrs systems;
  in {
    devShells = forEachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      default = pkgs.mkShell {
        name = "sweetsidle-dev";
        packages = with pkgs; [
          bash
          cargo
          cargo-deny
          clippy
          dbus
          git
          rust-analyzer
          rustc
          rustfmt
        ];
      };
    });

    formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.alejandra);
  };
}
