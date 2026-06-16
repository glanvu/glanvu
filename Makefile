.PHONY: build release app install-app linux-pkg windows-pkg clean

build:
	cargo build

release:
	cargo build --release

## macOS .app bundle
app:
	./scripts/build-macos-app.sh --release

## Install to /Applications + /usr/local/bin symlink (macOS only)
install-app: app
	rm -rf /Applications/Glanvu.app
	cp -r dist/macos/Glanvu.app /Applications/Glanvu.app
	/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
	  -f /Applications/Glanvu.app 2>/dev/null || true
	@echo "Installed: /Applications/Glanvu.app"
	@# Symlink so 'glanvu' works from any terminal (requires write permission to /usr/local/bin)
	ln -sf /Applications/Glanvu.app/Contents/MacOS/Glanvu /usr/local/bin/glanvu 2>/dev/null && \
	  echo "Terminal: glanvu command available" || \
	  echo "Run manually: sudo ln -sf /Applications/Glanvu.app/Contents/MacOS/Glanvu /usr/local/bin/glanvu"

## Linux .tar.gz + .deb (run on Linux or via Docker/CI)
linux-pkg: release
	./scripts/build-linux-pkg.sh

## Windows .zip (run on Windows or via CI)
windows-pkg: release
	./scripts/build-windows-pkg.sh

clean:
	cargo clean
	rm -rf dist/
