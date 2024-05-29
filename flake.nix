{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-utils.follows = "flake-utils";
      };
    };
    crane = {
      url = "github:ipetkov/crane";
      inputs = {
        nixpkgs.follows = "nixpkgs";
      };
    };
  };
  outputs = {
    self,
    nixpkgs,
    flake-utils,
    rust-overlay,
    crane,
  }:
    flake-utils.lib.eachDefaultSystem
    (
      system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rust-overlay)];
        };
        craneLib = crane.lib.${system};
        main = craneLib.buildPackage {
          pname = "mount-polaris";
          src = craneLib.cleanCargoSource ./.;
          nativeBuildInputs = [pkgs.pkg-config pkgs.fuse3];
        };
      in {
        packages.default = main;
        devShells.default = pkgs.mkShell {
          inputsFrom = [main];
          packages = with pkgs; [rust-analyzer rustfmt cargo-watch];
        };
        devShells.watch = pkgs.mkShell {
          inputsFrom = [main];
          packages = [pkgs.cargo-watch];
          shellHook = "exec cargo watch";
        };
      }
    );
}
