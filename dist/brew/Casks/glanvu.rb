cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.8.0"

  on_arm do
    sha256 "2aa7ea359c72f2c01c650fc043787d67220c4c2cf28d88b311821e5c902af8bc"
    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end

  on_intel do
    sha256 "89576f6de3d93a4462c05838f9a7225cb0056c117c973d98d42ed0f65af3373b"
    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-x86_64.zip"
  end

  name "Glanvu"
  desc "Fast, keyboard-driven, cross-platform universal image viewer and converter"
  homepage "https://glanvu.com"

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
