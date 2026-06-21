class Glanvu < Formula
  desc "Fast, keyboard-driven image viewer and batch converter"
  homepage "https://glanvu.com"
  license "Apache-2.0"

  on_linux do
    url "https://github.com/glanvu/glanvu/releases/download/v0.6.0/glanvu-0.6.0-linux-x86_64.tar.gz"
    sha256 "bbf393a3db0e94199b4268eba50d1bcf2567f0d59a411d97397b7e53d3b7f99b"
    version "0.6.0"
  end

  on_macos do
    on_arm do
      url "https://github.com/glanvu/glanvu/releases/download/v0.6.0/Glanvu-0.6.0-macos-arm64.zip"
      sha256 "9fe456bd24060076f1cc6baa8ade820ccd4c252e3e4d909cde567747d722c8fe"
      version "0.6.0"
    end

    on_intel do
      url "https://github.com/glanvu/glanvu/releases/download/v0.6.0/Glanvu-0.6.0-macos-x86_64.zip"
      sha256 "7ff52b75df4a94451774972f050e8255f66fb4083df08f571384eaa56cf74ab3"
      version "0.6.0"
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
