desktop_file = i18n.merge_file(
  input: 'io.github.linuxuser67.Grab.desktop.in',
  output: 'io.github.linuxuser67.Grab.desktop',
  type: 'desktop',
  po_dir: '../po',
  install: true,
  install_dir: get_option('datadir') / 'applications',
)

desktop_utils = find_program('desktop-file-validate', required: false)
if desktop_utils.found()
  test('Validate desktop file', desktop_utils,
    args: [desktop_file],
  )
endif

appstreamcli = find_program('appstreamcli', required: false)
if appstreamcli.found()
  test('Validate appstream file', appstreamcli,
    args: ['validate', '--no-net', '--explain',
      appstream_metainfo],
  )
endif

install_data('io.github.linuxuser67.Grab.gschema.xml',
  install_dir: get_option('datadir') / 'glib-2.0/schemas',
)

# D-Bus activation: required because the desktop file sets DBusActivatable=true.
# Without this, launching from Shell/the app grid silently does nothing.
configure_file(
  input: 'io.github.linuxuser67.Grab.service.in',
  output: 'io.github.linuxuser67.Grab.service',
  configuration: {'bindir': get_option('prefix') / get_option('bindir')},
  install_dir: get_option('datadir') / 'dbus-1' / 'services',
)

compile_schemas = find_program('glib-compile-schemas', required: false)
if compile_schemas.found()
  test('Validate schema file', compile_schemas,
    args: ['--strict', '--dry-run', meson.current_source_dir()],
  )
endif

subdir('icons')
