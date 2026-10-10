# Orchard's build image for this atomic host (the host ships no compiler).
# Same shape as kumaui's kuma-dev-rust: derived from kuma's kuma-dev-gcc,
# which tracks the Fedora the release targets, and adds only what the
# gpui native stack needs: fontconfig and freetype (text), xkbcommon
# (input), clang-devel (for the C glue some -sys crates generate), and
# pkg-config so those builds can find their libraries.
#
#   podman build -t localhost/orchard-dev-rust \
#     -f containers/orchard-dev-rust.containerfile containers/
FROM localhost/kuma-dev-gcc

RUN dnf -y install pkgconf-pkg-config fontconfig-devel freetype-devel \
      libxkbcommon-devel clang-devel \
  && dnf clean all
