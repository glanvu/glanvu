class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.6.1/glanvu-0.6.1-linux-x86_64.tar.gz"
    sha256 "36c4a0f58061bdc1025af0bccef05d9517774c4574ef47d442f80ec08ba1e322"
    version "0.6.1"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.6.1/Glanvu-0.6.1-macos-arm64.zip"
      sha256 "f680a3802f3ef102b007bd075f8266c2853149782984a981e5089aaba3209cfe"
      version "0.6.1"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.6.1/Glanvu-0.6.1-macos-x86_64.zip"
      sha256 "1f62a010acb3ef36a8f5f0ca6cf51753c4c7bf7dbd7bd0d807121133c53b6f56"
      version "0.6.1"
    end
  end

  def install
    if OS.mac?
      bin.install "Glanvu.app/Contents/MacOS/glanvu"
    else
      bin.install "glanvu"
    end
  end

  test do
    system bin/"glanvu", "--help"
  end
end
