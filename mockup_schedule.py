#!/usr/bin/env python3
"""Mockup of Grab's scheduled-download UI, mirroring src/inline_add.rs exactly."""
import os
import sys
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib

STATE = sys.argv[1] if len(sys.argv) > 1 else "on"  # "on" | "off"
SIGDIR = os.environ.get("MOCKUP_SIGDIR", "/tmp")

app = Adw.Application(application_id="io.github.Mockup")


def build(window):
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
    header = Adw.HeaderBar()
    header.set_title_widget(Gtk.Label(label="New Download"))
    box.append(header)

    clamp = Adw.Clamp(maximum_size=480, margin_top=18, margin_bottom=18,
                      margin_start=18, margin_end=18)
    box.append(clamp)
    group = Adw.PreferencesGroup()
    clamp.set_child(group)

    # Context rows (stand-ins for the real card's destination/name rows).
    dest_row = Adw.ActionRow(title="Destination")
    dest_row.add_suffix(Gtk.Label(label="~/Downloads"))
    group.add(dest_row)
    name_row = Adw.EntryRow(title="File name")
    name_row.set_text("report.pdf")
    group.add(name_row)

    # --- Schedule section: verbatim mirror of inline_add.rs ---
    schedule_switch = Adw.SwitchRow(title="Schedule download",
                                    subtitle="Start at a specific time")
    group.add(schedule_switch)

    schedule_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
    schedule_box.set_margin_top(6)
    schedule_box.set_margin_bottom(6)
    schedule_revealer = Gtk.Revealer(
        transition_type=Gtk.RevealerTransitionType.SLIDE_DOWN,
        reveal_child=False)
    schedule_revealer.set_child(schedule_box)

    calendar = Gtk.Calendar()
    date_popover = Gtk.Popover()
    date_popover.set_child(calendar)
    date_btn = Gtk.MenuButton(label="Choose date…", popover=date_popover)
    date_row = Adw.ActionRow(title="Date")
    date_row.add_suffix(date_btn)
    date_row.set_activatable_widget(date_btn)
    schedule_box.append(date_row)

    hour_spin = Adw.SpinRow(title="Hour",
                            adjustment=Gtk.Adjustment(value=14, lower=0, upper=23,
                                                      step_increment=1, page_increment=5))
    minute_spin = Adw.SpinRow(title="Minute",
                              adjustment=Gtk.Adjustment(value=30, lower=0, upper=59,
                                                        step_increment=1, page_increment=5))
    schedule_box.append(hour_spin)
    schedule_box.append(minute_spin)

    def update():
        dt = calendar.get_date()
        sched = GLib.DateTime.new_local(dt.get_year(), dt.get_month(),
                                        dt.get_day_of_month(), 14, 30, 0)
        date_btn.set_label(sched.format("%Y-%m-%d"))

    # Pick tomorrow so the mockup shows a real scheduled date.
    tomorrow = GLib.DateTime.new_now_local().add_days(1)
    calendar.select_day(tomorrow)
    update()

    schedule_switch.connect("notify::active",
                            lambda sw, _: schedule_revealer.set_reveal_child(sw.get_active()))
    group.add(schedule_revealer)
    # --- end mirror ---

    if STATE == "on":
        schedule_switch.set_active(True)

    window.set_content(box)
    window.set_default_size(480, 560)


def build_prefs(window):
    """Mirror of the Downloads preferences page with the new Scheduling group."""
    page = Adw.PreferencesPage(title="Downloads", icon_name="folder-download-symbolic")
    window.add(page)

    dest_group = Adw.PreferencesGroup(title="Destination")
    dest_group.add(Adw.ActionRow(title="Download location"))
    dest_group.add(Adw.SwitchRow(title="Restrict filenames to ASCII"))
    page.add(dest_group)

    # --- New: Scheduling group (mirrors preferences.rs) ---
    sched_group = Adw.PreferencesGroup(title="Scheduling")
    sched_group.add(Adw.SwitchRow(title="Scheduled downloads",
                                  subtitle="Allow downloads to start at a specific time"))
    page.add(sched_group)
    # --- end mirror ---

    power_group = Adw.PreferencesGroup(title="Power")
    power_group.add(Adw.SwitchRow(title="Prevent sleep during downloads"))
    page.add(power_group)

    window.set_default_size(560, 480)


def on_activate(app):
    if len(sys.argv) > 2 and sys.argv[2] == "prefs":
        win = Adw.PreferencesWindow(application=app)
        build_prefs(win)
    else:
        win = Adw.ApplicationWindow(application=app)
        build(win)
    win.present()
    # Signal readiness for the screenshot, then wait to be told to quit.
    open(os.path.join(SIGDIR, "mockup_ready"), "w").write("ready")
    GLib.timeout_add(500, lambda: (
        __import__("os")._exit(0) if __import__("os").path.exists(__import__("os").path.join(SIGDIR, "mockup_done")) else True
    ) and True)


app.connect("activate", on_activate)
app.run([])
