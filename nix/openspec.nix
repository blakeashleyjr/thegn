# Hermetic build of the OpenSpec CLI (@fission-ai/openspec) — the spec-driven
# development tool thegn uses to manage its OWN development (not a runtime
# dependency of thegn). Pinned and built from source with pnpm so both
# `nix develop` and `devenv shell` get an identical, offline-reproducible
# `openspec` with NO global npm install and telemetry off by construction.
#
# Bump deliberately: change `version` + `rev`, then re-resolve the two FODs by
# setting their hashes to lib.fakeHash and reading the expected values from the
# `nix build .#openspec` error.
{
  lib,
  stdenv,
  fetchFromGitHub,
  nodejs,
  pnpm,
  pnpmConfigHook,
  fetchPnpmDeps,
  makeWrapper,
}:
stdenv.mkDerivation (finalAttrs: {
  pname = "openspec";
  version = "1.6.0";

  src = fetchFromGitHub {
    owner = "Fission-AI";
    repo = "OpenSpec";
    rev = "v${finalAttrs.version}";
    hash = "sha256-lvg10gpx6tB6eSv5iesqhUQqYqkVuU4hpSVfYy/f3bE=";
  };

  nativeBuildInputs = [
    nodejs
    pnpm
    pnpmConfigHook
    makeWrapper
  ];

  # Fixed-output fetch of the full pnpm dependency closure (fetcherVersion 4 = pnpm 11+).
  pnpmDeps = fetchPnpmDeps {
    inherit (finalAttrs) pname version src;
    fetcherVersion = 4;
    hash = "sha256-+392kmJ9fWQZW4R7n3sompLirulmA5VfTUWH6IL9MBU=";
  };

  # The postinstall script only prints a shell-completions tip; CI=1 +
  # OPENSPEC_NO_COMPLETIONS=1 keep it silent and offline during the build.
  env.CI = "1";
  env.OPENSPEC_NO_COMPLETIONS = "1";

  buildPhase = ''
    runHook preBuild
    pnpm run build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib/openspec
    # dist + bin + schemas are the runtime payload; node_modules carries the
    # pnpm virtual store (relative symlinks under node_modules/.pnpm), so the
    # copied tree is self-contained.
    cp -r dist bin schemas package.json node_modules $out/lib/openspec/
    makeWrapper ${nodejs}/bin/node $out/bin/openspec \
      --add-flags "$out/lib/openspec/bin/openspec.js" \
      --set OPENSPEC_NO_COMPLETIONS 1 \
      --set-default OPENSPEC_TELEMETRY 0 \
      --set-default DO_NOT_TRACK 1
    runHook postInstall
  '';

  meta = {
    description = "Spec-driven development workflow CLI for AI coding agents";
    homepage = "https://github.com/Fission-AI/OpenSpec";
    license = lib.licenses.mit;
    mainProgram = "openspec";
  };
})
