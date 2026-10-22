cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.12.0"

  on_arm do
    sha256 "82be88800568faae44e4063d4834ce57258d5a9627ece50eed4cb8dd4dac8214"

    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end
  on_intel do
    sha256 "ef82d58db7f13e8cacf8f536fb7e0def18dcdf3295b0d935a0067fc869561a55"

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
