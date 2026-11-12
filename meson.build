project('grab', 'rust',
  version: '6.5.0-beta.13',
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

subdir('po')

# Install and embed the same translated AppStream document. The About dialog
# reads this metadata from the GResource compiled by build.rs.
appstream_metainfo = i18n.merge_file(
  input: 'data/io.github.linuxuser67.Grab.metainfo.xml.in',
  output: 'io.github.linuxuser67.Grab.metainfo.xml',
  type: 'xml',
  po_dir: 'po',
  install: true,
  install_dir: get_option('datadir') / 'metainfo',
)

subdir('data')
subdir('src')

gnome.post_install(
  glib_compile_schemas: true,
  gtk_update_icon_cache: true,
  update_desktop_database: true,
)
