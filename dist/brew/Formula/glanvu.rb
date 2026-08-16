class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.10.0/glanvu-0.10.0-linux-x86_64.tar.gz"
    sha256 "8290a637bd4078e012e4f9d8aae3d5fced519fe571aa5ec73f05fee23c0e04ef"
    version "0.10.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.10.0/Glanvu-0.10.0-macos-arm64.zip"
      sha256 "d6d5503893980d29892bf0076b17b597482ff05178eaa8f676281857f932594e"
      version "0.10.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.10.0/Glanvu-0.10.0-macos-x86_64.zip"
      sha256 "dd54173231dd5f4e482b32681a2f59e65c20e4944c6bdb707e02d7778debd3be"
      version "0.10.0"
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
