cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.5.0"

  on_arm do
    sha256 "51a8586eeac520ba97f02fe565bf037a42fd77c3a86d469a53bb7b410fa97f65"
    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end

  on_intel do
    sha256 "2057242093d1dbda92824fb37e982ff8d797a7cbce286f502fe1b07b18577353"
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
