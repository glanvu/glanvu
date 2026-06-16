cask "glanvu" do
  # Update version + sha256 on each release.
  # Generate sha256 with: shasum -a 256 Glanvu-<version>-macos-arm64.zip
  version "0.0.0"
  sha256 :no_check   # replace with real sha256 before publishing

  # GitHub Releases URL pattern (arm64 macOS zip):
  url "https://github.com/glanvu-dev/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  # Alternative: x86_64 build
  # url "https://github.com/glanvu-dev/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-x86_64.zip"

  name "Glanvu"
  desc "Fast, keyboard-driven, cross-platform universal image viewer and converter"
  homepage "https://glanvu.com"

  app "Glanvu.app"

  # Associate Glanvu with common image types in Finder (optional — user can override).
  # This runs lsregister so macOS knows Glanvu can open these files.
  postflight do
    system_command "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister",
                   args: ["-f", "#{appdir}/Glanvu.app"]
  end

  uninstall quit: "com.glanvu.app"

  zap trash: [
    "~/Library/Caches/glanvu",
    "~/Library/Preferences/com.glanvu.app.plist",
  ]
end
