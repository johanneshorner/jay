{
  rustPlatform,
  pkg-config,
  installShellFiles,
  cairo,
  libGL,
  libinput,
  mesa,
  pango,
  wayland,
  shaderc,
  vulkan-loader,
  xwayland,
  systemd,
  pipewire,
}:

rustPlatform.buildRustPackage {
  pname = "jay";
  version = "1.9.0";

  nativeBuildInputs = [
    pkg-config
    installShellFiles
  ];

  buildInputs = [
    cairo
    libinput
    mesa
    pango
    wayland
    xwayland
    systemd
    pipewire
  ];

  runtimeDependencies = [
    libGL
    vulkan-loader
  ];

  SHADERC_LIB_DIR = "${shaderc.lib}/lib";

  src = ./..;
  cargoHash = "sha256-/+BdhSoRgDGibcHb+zqmtGzRHFXEgfavpBD0FQ03kyI=";

  postInstall = ''
    installShellCompletion --cmd jay \
      --bash <($out/bin/jay generate-completion bash) \
      --fish <($out/bin/jay generate-completion fish) \
      --zsh <($out/bin/jay generate-completion zsh)
  '';

  meta = {
    description = "The jay wayland compositor";
  };
}
