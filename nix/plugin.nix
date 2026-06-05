# Build one superzej WASM zellij plugin (sidebar or panel).
#
# Plugins are standalone crates under plugin/<name>/ targeting wasm32-wasip1.
# The caller supplies a `rustPlatform` built from a toolchain that has the
# wasm32-wasip1 target (see flake.nix). The built `.wasm` is installed to
# $out/share/superzej/<wasmName> for the home-manager module to deploy.
{
  lib,
  rustPlatform,
  pname,
  src,
  wasmName,
}:
rustPlatform.buildRustPackage {
  inherit pname src;
  version = "0.1.0";

  cargoLock.lockFile = src + "/Cargo.lock";

  buildPhase = ''
    runHook preBuild
    cargo build --release --offline --target wasm32-wasip1
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/share/superzej
    cp target/wasm32-wasip1/release/${pname}.wasm $out/share/superzej/${wasmName}
    runHook postInstall
  '';

  # No tests run against the wasm artifact.
  doCheck = false;

  meta = {
    description = "superzej zellij plugin (${pname})";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
