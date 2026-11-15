PREFIX ?= /usr/local
DESTDIR ?=
CARGO ?= cargo
ICON_SIZES = 16 32 128 256 512

.PHONY: build install uninstall

build:
	$(CARGO) build --release

install:
	install -Dm755 target/release/luna $(DESTDIR)$(PREFIX)/bin/luna
	install -Dm644 data/us.priet.Luna.desktop $(DESTDIR)$(PREFIX)/share/applications/us.priet.Luna.desktop
	for s in $(ICON_SIZES); do \
		install -Dm644 data/hicolor/$${s}x$${s}/apps/us.priet.Luna.png \
			$(DESTDIR)$(PREFIX)/share/icons/hicolor/$${s}x$${s}/apps/us.priet.Luna.png; \
	done
	install -Dm644 LICENSE $(DESTDIR)$(PREFIX)/share/licenses/luna/LICENSE

uninstall:
	$(RM) $(DESTDIR)$(PREFIX)/bin/luna
	$(RM) $(DESTDIR)$(PREFIX)/share/applications/us.priet.Luna.desktop
	for s in $(ICON_SIZES); do \
		$(RM) $(DESTDIR)$(PREFIX)/share/icons/hicolor/$${s}x$${s}/apps/us.priet.Luna.png; \
	done
	$(RM) $(DESTDIR)$(PREFIX)/share/licenses/luna/LICENSE
