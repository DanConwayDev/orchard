# Orchard's build image (the host ships no compiler, and CI has no
# kuma tree to derive from): Fedora at the version the release targets,
# gcc for the C glue some -sys crates generate, and the gpui native
# stack: fontconfig and freetype (text), xkbcommon (input), clang-devel
# (for bindgen), and pkg-config so those builds find their libraries.
#
#   podman build -t localhost/orchard-dev-rust \
#     -f containers/orchard-dev-rust.containerfile containers/
FROM fedora:44

RUN dnf -y install gcc pkgconf-pkg-config fontconfig-devel freetype-devel \
      libxkbcommon-devel libxkbcommon-x11-devel clang-devel \
      flatpak python3 \
  && dnf clean all
