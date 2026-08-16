class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.9.1/glanvu-0.9.1-linux-x86_64.tar.gz"
    sha256 "3e23d502355f066ed5998eea895521348ea03610b38bee35b789fb3e69476164"
    version "0.9.1"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.9.1/Glanvu-0.9.1-macos-arm64.zip"
      sha256 "6062cbf6a1e1ec31f94b29abda504fd56a5a7cc43f4767d3d79632ad2cfa27d9"
      version "0.9.1"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.9.1/Glanvu-0.9.1-macos-x86_64.zip"
      sha256 "e5c80f5b677ff12691b2690db3e152aba90eb3a667dd0b7042e3be355f086b08"
      version "0.9.1"
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
