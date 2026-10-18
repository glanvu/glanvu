class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.11.0/glanvu-0.11.0-linux-x86_64.tar.gz"
    sha256 "93081c6ae53350ba0b236cb015c25ac7c652b9bdd42f78f34de267e1a320add7"
    version "0.11.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.11.0/Glanvu-0.11.0-macos-arm64.zip"
      sha256 "5bf817fe7473fd71dca161b670f486182db17fd406945c648744f16c62e95ea6"
      version "0.11.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.11.0/Glanvu-0.11.0-macos-x86_64.zip"
      sha256 "d1185102c5035940eb88a8e0b98b6e4699c5921dcbd0cf747946bfb254818ef7"
      version "0.11.0"
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
