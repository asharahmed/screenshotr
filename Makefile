.PHONY: dev app install release

dev:
	./dev.sh

app:
	./dev.sh

install:
	./scripts/install-macos.sh

release:
	./scripts/release.sh
