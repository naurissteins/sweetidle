{
  lib,
  rustPlatform,
  coreutils,
}: let
  cargoToml = lib.importTOML ../Cargo.toml;
in
  rustPlatform.buildRustPackage {
    pname = "sweetidle";
    inherit (cargoToml.package) version;

    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../src
        ../tests
        ../config
        ../man
        ../systemd
      ];
    };

    cargoLock.lockFile = ../Cargo.lock;

    postInstall = ''
      install -Dm644 man/sweetidle.1 -t $out/share/man/man1
      install -Dm644 config/example.toml -t $out/share/doc/sweetidle
      # systemd cannot find bare command names on NixOS
      install -Dm644 systemd/sweetidle.service -t $out/lib/systemd/user
      substituteInPlace $out/lib/systemd/user/sweetidle.service \
        --replace-fail "ExecStart=sweetidle" "ExecStart=$out/bin/sweetidle" \
        --replace-fail "ExecReload=kill" "ExecReload=${coreutils}/bin/kill"
    '';

    meta = {
      description = cargoToml.package.description;
      homepage = cargoToml.package.repository;
      license = lib.licenses.gpl3Plus;
      mainProgram = "sweetidle";
      platforms = lib.platforms.linux;
    };
  }
