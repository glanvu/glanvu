cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.11.0"

  on_arm do
    sha256 "5bf817fe7473fd71dca161b670f486182db17fd406945c648744f16c62e95ea6"

    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end
  on_intel do
    sha256 "d1185102c5035940eb88a8e0b98b6e4699c5921dcbd0cf747946bfb254818ef7"

    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-x86_64.zip"
  end

  name "Glanvu"
  desc "Fast, keyboard-driven, cross-platform universal image viewer and converter"
  homepage "https://glanvu.com/"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on :macos

  app "Glanvu.app"

  uninstall quit: "com.glanvu.app"

  zap trash: [
    "~/Library/Caches/glanvu",
    "~/Library/Preferences/com.glanvu.app.plist",
  ]
end
