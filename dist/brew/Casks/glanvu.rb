cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.5.3"

  on_arm do
    sha256 "e18bd9cc414b4aca915cf9dd583f5a5d0f692b35817e29123b96d9e98e615d04"
    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end

  on_intel do
    sha256 "fdf888a36d74db1dc5c9502b5e2e19138a54c5cb6432b9214347647f7f0e26bf"
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
