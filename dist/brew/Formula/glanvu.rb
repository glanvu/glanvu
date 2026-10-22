class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.12.0/glanvu-0.12.0-linux-x86_64.tar.gz"
    sha256 "4735e536de2b419d0af10239682832a30f628aff8fdee8bc9ca9f582ef990878"
    version "0.12.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.12.0/Glanvu-0.12.0-macos-arm64.zip"
      sha256 "82be88800568faae44e4063d4834ce57258d5a9627ece50eed4cb8dd4dac8214"
      version "0.12.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.12.0/Glanvu-0.12.0-macos-x86_64.zip"
      sha256 "ef82d58db7f13e8cacf8f536fb7e0def18dcdf3295b0d935a0067fc869561a55"
      version "0.12.0"
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
