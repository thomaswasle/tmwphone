# Maintainer: Thomas Müller-Wasle <mueller@loca.net>
pkgname=tmwphone
pkgver=0.1.0
pkgrel=1
pkgdesc="SIP softphone client for GNOME"
arch=('x86_64' 'aarch64')
url="https://github.com/thomaswasle/tmwphone"
license=('Apache-2.0')
depends=(
    'gtk4'
    'libadwaita'
    'glib2'
    'sofia-sip'
    'gstreamer'
    'gst-plugins-base'
    'gst-plugins-good'
    'libsecret'
)
makedepends=(
    'rust'
    'cargo'
    'pkg-config'
)
source=("$pkgname-$pkgver.tar.gz::https://github.com/thomaswasle/tmwphone/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
    cd "$pkgname-$pkgver"
    cargo build --release --locked
}

package() {
    cd "$pkgname-$pkgver"

    install -Dm755 target/release/tmwphone \
        "$pkgdir/usr/bin/tmwphone"

    install -Dm644 data/io.github.thomaswasle.TMWPhone.gschema.xml \
        "$pkgdir/usr/share/glib-2.0/schemas/io.github.thomaswasle.TMWPhone.gschema.xml"

    install -Dm644 data/io.github.thomaswasle.TMWPhone.desktop \
        "$pkgdir/usr/share/applications/io.github.thomaswasle.TMWPhone.desktop"

    install -Dm644 data/icons/io.github.thomaswasle.TMWPhone.svg \
        "$pkgdir/usr/share/icons/hicolor/scalable/apps/io.github.thomaswasle.TMWPhone.svg"
}
