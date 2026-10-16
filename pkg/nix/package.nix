{
  lib,
  rustPlatform,
  pkg-config,
  cmake,
  clang,
  makeWrapper,
  ffmpeg-headless,
  libdrm,
  libgbm,
  libva,
  libxkbcommon,
  mesa,
  pipewire,
  pam,
  pulseaudio,
  wayland,
}:

let
  cargoToml = builtins.fromTOML (builtins.readFile ../../Cargo.toml);
in
rustPlatform.buildRustPackage {
  pname = "hypr-rdp";
  version = cargoToml.package.version;

  src = lib.cleanSource ../..;

  cargoHash = "sha256-Yl+Id/qsS5nvHgoDaBELbSiBV/MzrWft7MXn/1Jr618=";

  nativeBuildInputs = [
    pkg-config
    cmake
    clang
    makeWrapper
    rustPlatform.bindgenHook
  ];

  # NVENC is loaded from the NVIDIA driver at runtime; without one, auto falls back.
  buildFeatures = [ "nvenc" ];

  buildInputs = [
    ffmpeg-headless
    libdrm
    libgbm
    libva
    libxkbcommon
    mesa
    pipewire
    pam
    wayland
  ];

  postInstall = ''
    install -Dm644 LICENSE $out/share/licenses/hypr-rdp/LICENSE
    wrapProgram $out/bin/hypr-rdp \
      --run 'if test -z "''${FUSERMOUNT_PATH-}" && test -x /run/wrappers/bin/fusermount3; then export FUSERMOUNT_PATH=/run/wrappers/bin/fusermount3; fi' \
      --prefix PATH : ${lib.makeBinPath [ pulseaudio ]}
  '';

  doCheck = false;

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/MuNeNICK/hypr-rdp";
    license = lib.licenses.mit;
    mainProgram = "hypr-rdp";
    platforms = lib.platforms.linux;
  };
}
