#!/bin/bash

rm -rf AppDir *.AppImage *.zsync
mkdir AppDir

install -Dsm755 -t AppDir/usr/bin target/release/wezterm-mux-server
install -Dsm755 -t AppDir/usr/bin target/release/wezterm
install -Dsm755 -t AppDir/usr/bin target/release/wezterm-gui
install -Dsm755 -t AppDir/usr/bin target/release/strip-ansi-escapes
install -Dm644 assets/icon/terminal.png AppDir/usr/share/icons/hicolor/128x128/apps/org.wezfurlong.wezterm.png
install -Dm644 assets/wezterm.desktop AppDir/usr/share/applications/org.wezfurlong.wezterm.desktop
install -Dm644 assets/wezterm.appdata.xml AppDir/usr/share/metainfo/org.wezfurlong.wezterm.appdata.xml

[ -x /tmp/linuxdeploy ] || ( curl -L 'https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage' -o /tmp/linuxdeploy && chmod +x /tmp/linuxdeploy )

TAG_NAME=${TAG_NAME:-$(git describe --tags --match '20*')}
TAG_NAME=${TAG_NAME:-$(date +'%Y%m%d-%H%M%S')-$(git log --format=%h -1)}
distro=$(lsb_release -is)
distver=$(lsb_release -rs)

# Embed appropriate update info
# https://github.com/AppImage/AppImageSpec/blob/master/draft.md#github-releases
if [[ "$BUILD_REASON" == "Schedule" ]] ; then
  UPDATE="gh-releases-zsync|wez|wezterm|nightly|WezTerm-*.AppImage.zsync"
  OUTPUT=WezTerm-nightly-$distro$distver.AppImage
else
  UPDATE="gh-releases-zsync|wez|wezterm|latest|WezTerm-*.AppImage.zsync"
  OUTPUT=WezTerm-$TAG_NAME-$distro$distver.AppImage
fi

# Munge the path so that it finds our appstreamcli wrapper
PATH="$PWD/ci:$PATH" \
VERSION="$TAG_NAME" \
UPDATE_INFORMATION="$UPDATE" \
OUTPUT="$OUTPUT" \
  /tmp/linuxdeploy \
  --appdir AppDir \
  --output appimage \
  --desktop-file assets/wezterm.desktop

# Update the AUR build file.  We only really want to use this for tagged
# builds but it doesn't hurt to generate it always here.
SHA256=$(sha256sum $OUTPUT | cut -d' ' -f1)
sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$SHA256/g" < ci/PKGBUILD.template > PKGBUILD
sed -e "s/@TAG@/$TAG_NAME/g" -e "s/@SHA256@/$SHA256/g" < ci/wezterm-linuxbrew.rb.template > wezterm-linuxbrew.rb
