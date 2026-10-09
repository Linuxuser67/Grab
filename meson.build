project('grab', 'rust',
  version: '5.6.9',
  meson_version: '>= 0.59.0',
  license: 'GPL-3.0-only',
)

# Flathub requires each module's license under share/licenses/$APP_ID.
install_data('LICENSE',
  install_dir: get_option('datadir') / 'licenses' / 'io.github.linuxuser67.Grab',
)

gnome = import('gnome')
i18n = import('i18n')

cargo_sh = files('build-aux/cargo.sh')

subdir('data')
subdir('po')
subdir('src')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
