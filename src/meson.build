# Fetched cargo sources must be vendored for Flatpak (see build-aux/).
cargo_build = custom_target('cargo-build',
  build_by_default: true,
  build_always_stale: true,
  output: ['grab'],
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  command: [
    cargo_sh, meson.project_source_root(), '@OUTPUT0@',
    get_option('buildtype') == 'release' ? 'release' : 'debug',
  ],
)
