class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.5.4/glanvu-0.5.4-linux-x86_64.tar.gz"
    sha256 "89784c67f149b925beac1b7208d8961485092253ad66ad2ff460eadccfcd0b52"
    version "0.5.4"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.5.4/Glanvu-0.5.4-macos-arm64.zip"
      sha256 "975844e0b6f2ee65dbfecdcdc3e759814164ab9abd03c2ce04aa9a36123782e5"
      version "0.5.4"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.5.4/Glanvu-0.5.4-macos-x86_64.zip"
      sha256 "2f63998127629cf8a80e43dca6a5c466347d020d73e87a3f1f0118090ed705a0"
      version "0.5.4"
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
