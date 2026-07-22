Name:       sehcontrol
Version:    1.1.9
Release:    0
Summary:    RPM package
License:    GPL-3.0
Requires:   gtk3 libxcb1 libXfixes3 alsa-utils libXtst6 libva2 pam gstreamer-plugins-base gstreamer-plugin-pipewire
Recommends: libayatana-appindicator3-1 xdotool

# https://docs.fedoraproject.org/en-US/packaging-guidelines/Scriptlets/

%description
The best open-source remote desktop client software, written in Rust.

%prep
# we have no source, so nothing here

%build
# we have no source, so nothing here

%global __python %{__python3}

%install
mkdir -p %{buildroot}/usr/bin/
mkdir -p %{buildroot}/usr/share/sehcontrol/
mkdir -p %{buildroot}/usr/share/sehcontrol/files/
mkdir -p %{buildroot}/usr/share/icons/hicolor/256x256/apps/
mkdir -p %{buildroot}/usr/share/icons/hicolor/scalable/apps/
install -m 755 $HBB/target/release/sehcontrol %{buildroot}/usr/bin/sehcontrol
install $HBB/libsciter-gtk.so %{buildroot}/usr/share/sehcontrol/libsciter-gtk.so
install $HBB/res/sehcontrol.service %{buildroot}/usr/share/sehcontrol/files/
install $HBB/res/128x128@2x.png %{buildroot}/usr/share/icons/hicolor/256x256/apps/sehcontrol.png
install $HBB/res/scalable.svg %{buildroot}/usr/share/icons/hicolor/scalable/apps/sehcontrol.svg
install $HBB/res/sehcontrol.desktop %{buildroot}/usr/share/sehcontrol/files/
install $HBB/res/sehcontrol-link.desktop %{buildroot}/usr/share/sehcontrol/files/

%files
/usr/bin/sehcontrol
/usr/share/sehcontrol/libsciter-gtk.so
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
    rm /usr/share/applications/sehcontrol.desktop || true
    rm /usr/share/applications/sehcontrol-link.desktop || true
    update-desktop-database
  ;;
  1)
    # for upgrade
  ;;
esac
