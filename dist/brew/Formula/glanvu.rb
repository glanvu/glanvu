class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.5.0/glanvu-0.5.0-linux-x86_64.tar.gz"
    sha256 "e9a462f2dd735f3e6034d40022a03e5af9ed92e2f0198d3047e3dab3ecd9f626"
    version "0.5.0"
  end

  on_macos do
    url "https://github.com/glanvu/glanvu/releases/download/v0.5.0/Glanvu-0.5.0-macos-arm64.zip"
    sha256 "51a8586eeac520ba97f02fe565bf037a42fd77c3a86d469a53bb7b410fa97f65"
    version "0.5.0"
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
