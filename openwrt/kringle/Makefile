# OpenWrt package for the Kringle server.
#
# The binary is cross-compiled with cargo (see scripts/build-openwrt.sh) and
# dropped into files/ before the SDK runs: building Rust inside the SDK would
# mean bootstrapping a whole Rust toolchain for a pure-Rust static binary.

include $(TOPDIR)/rules.mk

PKG_NAME:=kringle
PKG_VERSION:=0.1.1
PKG_RELEASE:=1

PKG_LICENSE:=GPL-3.0-or-later

include $(INCLUDE_DIR)/package.mk

define Package/kringle
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Web Servers/Proxies
  TITLE:=Kris Kringle / Secret Santa gift swap server
  DEPENDS:=@(aarch64)
endef

define Package/kringle/description
  A small gift swap web app: a host starts a group, shares one invite link,
  sets "keep apart" rules and draws names. No accounts. One static binary
  with its data in a single JSON file.
endef

define Package/kringle/conffiles
/etc/config/kringle
endef

define Build/Compile
endef

define Package/kringle/install
	$(INSTALL_DIR) $(1)/usr/bin $(1)/etc/init.d $(1)/etc/config $(1)/lib/upgrade/keep.d
	$(INSTALL_BIN) $(CURDIR)/files/kringle $(1)/usr/bin/kringle
	$(INSTALL_BIN) $(CURDIR)/files/kringle.init $(1)/etc/init.d/kringle
	$(INSTALL_CONF) $(CURDIR)/files/kringle.config $(1)/etc/config/kringle
	$(INSTALL_DATA) $(CURDIR)/files/kringle.keep $(1)/lib/upgrade/keep.d/kringle
endef

$(eval $(call BuildPackage,kringle))
