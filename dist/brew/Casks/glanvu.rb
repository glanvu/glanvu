cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.9.0"

  on_arm do
    sha256 "12710851b445adee31029c622e6ceb4d501968c90ad2882ebeb3367a8b306878"
    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end

  on_intel do
    sha256 "6d4347e98a4ed9e88708abe45d54c12ddb2cbba9e2a17a425c5c6390edb9bd4d"
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
