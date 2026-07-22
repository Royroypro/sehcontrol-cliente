Name:       sehcontrol
Version:    1.4.9
Release:    0
Summary:    RPM package
License:    GPL-3.0
URL:        https://rustdesk.com
Vendor:     sehcontrol <info@sehcontrol.com>
Requires:   gtk3 libxcb libXfixes alsa-lib libva pam gstreamer1-plugins-base
Recommends: libayatana-appindicator-gtk3 libxdo
Provides:   libdesktop_drop_plugin.so()(64bit), libdesktop_multi_window_plugin.so()(64bit), libfile_selector_linux_plugin.so()(64bit), libflutter_custom_cursor_plugin.so()(64bit), libflutter_linux_gtk.so()(64bit), libscreen_retriever_plugin.so()(64bit), libtray_manager_plugin.so()(64bit), liburl_launcher_linux_plugin.so()(64bit), libwindow_manager_plugin.so()(64bit), libwindow_size_plugin.so()(64bit), libtexture_rgba_renderer_plugin.so()(64bit)

# https://docs.fedoraproject.org/en-US/packaging-guidelines/Scriptlets/

%description
The best open-source remote desktop client software, written in Rust.

%prep
# we have no source, so nothing here

%build
# we have no source, so nothing here

# %global __python %{__python3}

%install

mkdir -p "%{buildroot}/usr/share/sehcontrol" && cp -r ${HBB}/flutter/build/linux/x64/release/bundle/* -t "%{buildroot}/usr/share/sehcontrol"
mkdir -p "%{buildroot}/usr/bin"
install -Dm 644 $HBB/res/sehcontrol.service -t "%{buildroot}/usr/share/sehcontrol/files"
install -Dm 644 $HBB/res/sehcontrol.desktop -t "%{buildroot}/usr/share/sehcontrol/files"
install -Dm 644 $HBB/res/sehcontrol-link.desktop -t "%{buildroot}/usr/share/sehcontrol/files"
install -Dm 644 $HBB/res/128x128@2x.png "%{buildroot}/usr/share/icons/hicolor/256x256/apps/sehcontrol.png"
install -Dm 644 $HBB/res/scalable.svg "%{buildroot}/usr/share/icons/hicolor/scalable/apps/sehcontrol.svg"

%files
/usr/share/sehcontrol/*
/usr/share/sehcontrol/files/sehcontrol.service
/usr/share/icons/hicolor/256x256/apps/sehcontrol.png
/usr/share/icons/hicolor/scalable/apps/sehcontrol.svg
/usr/share/sehcontrol/files/sehcontrol.desktop
/usr/share/sehcontrol/files/sehcontrol-link.desktop

%changelog
# let's skip this for now

%pre
# can do something for centos7
case "$1" in
  1)
    # for install
  ;;
  2)
    # for upgrade
    systemctl stop sehcontrol || true
  ;;
esac

%post
cp /usr/share/sehcontrol/files/sehcontrol.service /etc/systemd/system/sehcontrol.service
cp /usr/share/sehcontrol/files/sehcontrol.desktop /usr/share/applications/
cp /usr/share/sehcontrol/files/sehcontrol-link.desktop /usr/share/applications/
ln -sf /usr/share/sehcontrol/sehcontrol /usr/bin/sehcontrol
systemctl daemon-reload
systemctl enable sehcontrol
systemctl start sehcontrol
update-desktop-database

%preun
case "$1" in
  0)
    # for uninstall
    systemctl stop sehcontrol || true
    systemctl disable sehcontrol || true
    rm /etc/systemd/system/sehcontrol.service || true
  ;;
  1)
    # for upgrade
  ;;
esac

%postun
case "$1" in
  0)
    # for uninstall
    rm /usr/bin/sehcontrol || true
    rmdir /usr/lib/sehcontrol || true
    rmdir /usr/local/sehcontrol || true
    rmdir /usr/share/sehcontrol || true
    rm /usr/share/applications/sehcontrol.desktop || true
    rm /usr/share/applications/sehcontrol-link.desktop || true
    update-desktop-database
  ;;
  1)
    # for upgrade
    rmdir /usr/lib/sehcontrol || true
    rmdir /usr/local/sehcontrol || true
  ;;
esac
