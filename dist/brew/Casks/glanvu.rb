cask "glanvu" do
  # Update version + both sha256 values on each release (use scripts/bump-packaging.sh).
  version "0.10.0"

  on_arm do
    sha256 "d6d5503893980d29892bf0076b17b597482ff05178eaa8f676281857f932594e"

    url "https://github.com/glanvu/glanvu/releases/download/v#{version}/Glanvu-#{version}-macos-arm64.zip"
  end
  on_intel do
    sha256 "dd54173231dd5f4e482b32681a2f59e65c20e4944c6bdb707e02d7778debd3be"

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
