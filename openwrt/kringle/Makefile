# OpenWrt package for the Kringle server, with the LuCI page that sets it up
# (Services > Kringle). One package, like Monokulo's and Handicrafts'.
#
# The binary is cross-compiled with cargo (see scripts/build-openwrt.sh) and
# dropped into files/ before the SDK runs: building Rust inside the SDK would
# mean bootstrapping a whole Rust toolchain for one static binary.

include $(TOPDIR)/rules.mk

PKG_NAME:=kringle
PKG_VERSION:=0.3.0
PKG_RELEASE:=1

PKG_LICENSE:=GPL-3.0-only

include $(INCLUDE_DIR)/package.mk

define Package/kringle
  SECTION:=net
  CATEGORY:=Network
  SUBMENU:=Web Servers/Proxies
  TITLE:=Kris Kringle / Secret Santa gift swap server
  # A static binary. ca-bundle: Tern's Tuta SDK checks Tuta's certificate
  # against the system's CA store (SMTP carries its own roots).
  DEPENDS:=@(aarch64) +ca-bundle
  # The LuCI page used to be its own package.
  PROVIDES:=luci-app-kringle
  # Runs as its own user; created on install by the package scripts.
  USERID:=kringle:kringle
endef

define Package/kringle/description
  A small gift swap web app: a host starts a group, shares one invite link,
  sets "keep apart" rules and draws names. No accounts. Optional email
  through an SMTP server or a Tuta account. One static binary with its data
  in a single JSON file. Includes its LuCI page under Services > Kringle.
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
	$(INSTALL_DIR) $(1)/www/luci-static/resources/view
	$(INSTALL_DATA) $(CURDIR)/files/luci/kringle.js $(1)/www/luci-static/resources/view/kringle.js
	$(INSTALL_DIR) $(1)/usr/share/luci/menu.d $(1)/usr/share/rpcd/acl.d
	$(INSTALL_DATA) $(CURDIR)/files/luci/menu.json $(1)/usr/share/luci/menu.d/luci-app-kringle.json
	$(INSTALL_DATA) $(CURDIR)/files/luci/acl.json $(1)/usr/share/rpcd/acl.d/luci-app-kringle.json
endef

# LuCI reads its menu and ACLs at login; drop its caches so Services > Kringle
# shows up (or goes) without a reboot.
define Package/kringle/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] && exit 0
rm -f /tmp/luci-indexcache* /tmp/luci-modulecache/* 2>/dev/null
/etc/init.d/rpcd reload 2>/dev/null
exit 0
endef

define Package/kringle/postrm
#!/bin/sh
[ -n "$${IPKG_INSTROOT}" ] && exit 0
rm -f /tmp/luci-indexcache* /tmp/luci-modulecache/* 2>/dev/null
/etc/init.d/rpcd reload 2>/dev/null
exit 0
endef

$(eval $(call BuildPackage,kringle))
