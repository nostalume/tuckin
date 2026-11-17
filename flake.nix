{
  description = "Manage dotfiles across machines";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      release = {
        version = "0.1.1";
        archives = {
          x86_64-linux = {
            platform = "linux-x86_64";
            hash = "sha256-U5oX6Zt0E7R8e4q92JJMoRBdk+UoT4+aplEw+Gt2NCc=";
          };
          aarch64-darwin = {
            platform = "macos-aarch64";
            hash = "sha256-Q2ynZOBhxBLjacS/mq+w2LAYxrsTln/9pvpMxPhNSig=";
          };
        };
      };
    in {
      packages = nixpkgs.lib.genAttrs (builtins.attrNames release.archives) (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          archive = release.archives.${system};
          linux = pkgs.stdenv.hostPlatform.isLinux;
        in rec {
          default = if release.version == manifest.package.version then binary else source;
          binary = pkgs.stdenv.mkDerivation {
            pname = "tuckin-bin";
            inherit (release) version;
            src = pkgs.fetchurl {
              url = "https://github.com/nostalume/tuckin/releases/download/v${release.version}/tuckin-${archive.platform}.tar.gz";
              inherit (archive) hash;
            };
            sourceRoot = ".";
            nativeBuildInputs = pkgs.lib.optionals linux [ pkgs.autoPatchelfHook ];
            buildInputs = pkgs.lib.optionals linux [ pkgs.stdenv.cc.cc.lib ];
            dontConfigure = true;
            dontBuild = true;
            dontStrip = true;
            installPhase = ''
              runHook preInstall
              install -Dm755 tuckin "$out/bin/tuckin"
              install -Dm644 LICENSE "$out/share/licenses/tuckin/LICENSE"
              runHook postInstall
            '';
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              test "$("$out/bin/tuckin" --version)" = "tuckin ${release.version}"
              runHook postInstallCheck
            '';
            meta.mainProgram = "tuckin";
          };

          source = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            nativeCheckInputs = [ pkgs.git ];
            meta.mainProgram = "tuckin";
          };
        });
    };
}
