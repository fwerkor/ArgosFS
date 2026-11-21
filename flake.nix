{
  description = "ArgosFS: self-driving, erasure-coded Linux filesystem";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: builtins.listToAttrs (map (system: {
        name = system;
        value = f system;
      }) systems);
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
    in {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in rec {
          argosfs = pkgs.rustPlatform.buildRustPackage {
            pname = "argosfs";
            inherit version;
            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.fuse3 ];

            # Rootfs systemd/initramfs units are shipped as examples, not
            # enabled automatically on an arbitrary user's root filesystem.
            postInstall = ''
              mkdir -p "$out/share/argosfs"
              cp -r integrations docs "$out/share/argosfs/"
            '';

            cargoTestFlags = [ "--lib" ];

            meta = with pkgs.lib; {
              description = "Self-driving erasure-coded Linux filesystem";
              homepage = "https://argosfs.org";
              license = licenses.asl20;
              platforms = platforms.linux;
              mainProgram = "argosfs";
            };
          };
          default = argosfs;
        });

      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/argosfs";
        };
      });

      devShells = eachSystem (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config fuse3 ];
          };
        });
    };
}
