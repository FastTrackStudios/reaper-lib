{
  description = "reaper-lib — FTS REAPER UI lib (SWELL + blitz)";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fts-repo.url = "git+https://git.starcommand.live/FastTrackStudios/fts-repo";
    fts-repo.inputs.nixpkgs.follows = "nixpkgs";
  };
  nixConfig = {
    extra-trusted-public-keys = [ "fasttrackstudio.cachix.org-1:r7v7WXBeSZ7m5meL6w0wttnvsOltRvTpXeVNItcy9f4=" ];
    extra-substituters = [ "https://fasttrackstudio.cachix.org" ];
  };
  outputs = { self, nixpkgs, flake-utils, fts-repo }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system: {
      devShells.ci = fts-repo.lib.mkDevShell {
        inherit system;
        extraPackages = pkgs: fts-repo.lib.ftsUiBuildInputs pkgs;
      };
      devShells.default = self.devShells.${system}.ci;
    });
}
