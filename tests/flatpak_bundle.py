import configparser
import gzip
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

import gi

gi.require_version("Flatpak", "1.0")
from gi.repository import Flatpak, Gio

bundle, desktop_path = sys.argv[1:]
ref = Flatpak.BundleRef.new(Gio.File.new_for_path(bundle))
metadata = ref.get_appstream()
assert metadata is not None, "The Flatpak bundle has no embedded AppStream catalogue"
component = ET.fromstring(gzip.decompress(metadata.get_data())).find("component")
assert component is not None
assert component.findtext("id") == ref.get_name()
assert component.findtext("name") == "Archive Scout"
assert component.findtext("developer/name") == "skyarse"
desktop_id = component.findtext("launchable[@type='desktop-id']")
assert desktop_id == Path(desktop_path).name
assert any(ref.get_icon(size) is not None for size in (128, 64)), "Missing bundled icon"

desktop = configparser.ConfigParser(interpolation=None)
assert desktop.read(desktop_path)
assert desktop["Desktop Entry"]["Name"] == component.findtext("name")
command = desktop["Desktop Entry"]["Exec"]
assert "flatpak run " in command and "--command=archive-search" in command
assert ref.get_name() in command and f"--branch={ref.get_branch()}" in command
