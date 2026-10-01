include $(TOPDIR)/rules.mk

LUCI_TITLE:=LuCI support for Kringle
LUCI_DESCRIPTION:=Services > Kringle: port, listen address, public link address and data retention for the Kringle gift swap server.
LUCI_DEPENDS:=+kringle
# Runtime-only: every LuCI install has luci-base, and declaring it as a build
# dependency would make the SDK compile LuCI's whole stack to package 3 KB of JS.
LUCI_EXTRA_DEPENDS:=luci-base (>=0)
LUCI_PKGARCH:=all

PKG_VERSION:=0.1.1
PKG_RELEASE:=1
PKG_LICENSE:=GPL-3.0-or-later

include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature
