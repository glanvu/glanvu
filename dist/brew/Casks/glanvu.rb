cask "glanvu" do
  # Update version + sha256 on each release.
  # Generate sha256 with: shasum -a 256 Glanvu-<version>-macos-arm64.zip
  # (or: gh release view v<version> --json assets --jq '.assets[].digest')
  version "0.5.0"
  sha256 "51a8586eeac520ba97f02fe565bf037a42fd77c3a86d469a53bb7b410fa97f65"

  url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  name "Glanvu"
  desc "Fast, keyboard-driven, cross-platform universal image viewer and converter"
  homepage "https://glanvu.com"

  # Only an arm64 (Apple Silicon) build is published today; Intel users build from source.
  depends_on arch: :arm64

  livecheck do
    url :url
    strategy :github_latest
  end

  app "Glanvu.app"

  # Register Glanvu with Launch Services so Finder offers it under "Open With".
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
